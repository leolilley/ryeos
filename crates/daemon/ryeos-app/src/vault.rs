//! Node-vault: operator-secret store.
//!
//! ## Architectural role
//!
//! The daemon owns a single shared secret store. Delivering launches read
//! declared secrets only through [`authorize_secret_delivery`], after their
//! admission was verified and under the sealed environment authority of that
//! admission. The authorized values travel as a names-only contract into the
//! plan and join only the final process request.
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
//! - The daemon host environment and unpinned `.env` files are not secret
//!   sources. A project overlay is read only from its pinned, identity-checked
//!   root when the admitted environment authority names it.
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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};


pub mod external_channel;
pub mod external_connector;
pub mod external_relay_bootstrap;
pub mod placement;
use external_channel::ExternalChannelAuthorityAccess;
use external_connector::ExternalConnectorCapabilityAccess;
use external_relay_bootstrap::{ExternalRelayBootstrapAccess, ProtectedExternalRelayBootstrap};
use placement::PlacementCredentialAccess;

// Vault key-name policy + write helpers live in
// `ryeos_core_tools::actions::vault` so they can be shared with the CLI
// `ryeos vault {put,list,remove,rewrap}` verbs without a circular
// crate dependency. We re-export the pieces public callers (tests,
// fixtures, dispatch) need so this module's surface is unchanged.
pub use ryeos_vault::paths::default_sealed_store_path;
pub use ryeos_vault::policy::{
    BLOCKED_NAMES, INTERNAL_RUNTIME_VAULT_PREFIX, MAX_VAULT_ENTRIES, MAX_VAULT_ENVELOPE_BYTES,
    MAX_VAULT_KEY_BYTES, MAX_VAULT_PLAINTEXT_BYTES, MAX_VAULT_VALUE_BYTES,
    is_internal_runtime_vault_key, is_internal_vault_key, validate_decrypted_keys,
    validate_key_name, validate_secret_value,
};
pub use ryeos_vault::sealed::{recover_rewrap, with_store_lock, write_sealed_secrets};

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

fn validate_operator_secret_name(name: &str) -> Result<()> {
    if is_internal_runtime_vault_key(name) {
        bail!("vault: secret name uses reserved internal runtime vault prefix");
    }
    if is_internal_vault_key(name) {
        bail!("vault: secret name uses reserved internal placement vault prefix");
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

mod read_all_token {
    /// Proof that a whole-store read originates inside this crate.
    pub struct CrateOnly(pub(super) ());
}

/// Crate-private whole-store read. Delivering callers outside this crate
/// receive declared names only through the authority readers.
pub(crate) trait NodeVaultReadAll {
    fn read_all(&self, principal: &str) -> Result<HashMap<String, String>>;
}

impl<V: NodeVault + ?Sized> NodeVaultReadAll for V {
    fn read_all(&self, principal: &str) -> Result<HashMap<String, String>> {
        self.read_all_entries(principal, read_all_token::CrateOnly(()))
    }
}

/// Read-only operator-secret store. Daemon-owned, swappable backend.
pub trait NodeVault: Send + Sync + std::fmt::Debug {
    /// Seal the exact original file-relay bootstrap before first activation
    /// contact. A retry may only find identical canonical bytes.
    fn retain_external_relay_bootstrap(
        &self,
        _access: &ExternalRelayBootstrapAccess,
        _bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        _signed_import: &ryeos_external_execution_contract::guest_import_authorization::SignedGuestImportAuthorization,
    ) -> Result<()> {
        bail!("vault backend does not support protected external relay bootstrap")
    }

    /// Read only the original sealed bootstrap; this does not authorize guest
    /// input reconstruction or a second provider activation.
    fn external_relay_bootstrap(
        &self,
        _access: &ExternalRelayBootstrapAccess,
    ) -> Result<ProtectedExternalRelayBootstrap> {
        bail!("vault backend does not support protected external relay bootstrap")
    }

    /// Retrieve one app-private local connector capability generation. It is
    /// distinct from the remote supervisor bootstrap and never appears in
    /// operator/runtime secret enumeration.
    fn external_connector_capability(
        &self,
        _access: &ExternalConnectorCapabilityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        bail!("vault backend does not support protected external connector capability")
    }

    /// Insert-only generation provisioning. No overwrite or deletion API is
    /// available to runtime, project, provider, or operator-secret callers.
    fn provision_external_connector_capability(
        &self,
        _access: &ExternalConnectorCapabilityAccess,
        _value: &str,
    ) -> Result<()> {
        bail!("vault backend does not support protected external connector capability")
    }

    /// Atomically load or create the exact immutable local connector secret.
    fn ensure_external_connector_capability(
        &self,
        _access: &ExternalConnectorCapabilityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        bail!("vault backend does not support protected external connector capability")
    }

    /// Retrieve one app-private controller signer/bootstrap generation.  It is
    /// never exposed through operator/runtime secret enumeration.
    fn external_channel_authority(
        &self,
        _access: &ExternalChannelAuthorityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        bail!("vault backend does not support protected external channel authority")
    }

    /// Insert-only generation provisioning.  No overwrite or deletion API is
    /// available to runtime, project or operator-secret callers.
    fn provision_external_channel_authority(
        &self,
        _access: &ExternalChannelAuthorityAccess,
        _value: &str,
    ) -> Result<()> {
        bail!("vault backend does not support protected external channel authority")
    }

    /// Atomically load or create the exact immutable generation.  This avoids
    /// a read-then-create race and ensures a crash cannot reserve an allocation
    /// whose bootstrap plaintext was never durably sealed.
    fn ensure_external_channel_authority(
        &self,
        _access: &ExternalChannelAuthorityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        bail!("vault backend does not support protected external channel authority")
    }

    /// Only app code can construct this coordinate; callers must keep it inside
    /// the protected placement owner, never construct it from workload inputs.
    /// Unsupported backends refuse; never fall back to workload or host secrets.
    fn placement_credential(
        &self,
        _access: &PlacementCredentialAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        bail!("vault backend does not support protected placement credentials")
    }

    /// Insert-only generation provisioning. No overwrite or deletion API exists.
    fn provision_placement_credential(
        &self,
        _access: &PlacementCredentialAccess,
        _value: &str,
    ) -> Result<()> {
        bail!("vault backend does not support protected placement credentials")
    }
    /// Return the secrets the given principal is allowed to see.
    ///
    /// Only this crate can call it: the token is unnameable elsewhere, and
    /// in-crate callers use [`NodeVaultReadAll::read_all`]. External callers
    /// go through the authority readers, which return declared names only.
    fn read_all_entries(
        &self,
        principal: &str,
        token: read_all_token::CrateOnly,
    ) -> Result<HashMap<String, String>>;

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
         The item declares these in `required_secrets` but the admitted \
         environment authority does not provide them. Use `ryeos vault put` \
         for the operator vault or the project's `.env` when the launch \
         admits a project overlay, or remove the declaration.",
        names.join(", ")
    )]
    MissingSecrets {
        principal: String,
        names: Vec<String>,
    },
    /// Deterministic refusal: the admitted launch metadata itself forbids
    /// this read (declared-name authority, overlay identity). Retrying with
    /// the same admitted authority can never succeed, so callers may treat
    /// this as final — unlike [`VaultReadError::Internal`], which covers
    /// transient IO/config faults.
    #[error("vault authority violation: {0}")]
    AuthorityViolation(String),
    #[error("vault read error: {0}")]
    Internal(#[from] anyhow::Error),
}

/// Authorize and read the declared secrets for one delivering launch.
///
/// The admitted environment authority is the only source: its name authority
/// bounds the declared names, a project overlay must match its pinned root
/// identity, a vault authority names only the operator namespace, and a
/// delegated provider is refused. Neither the daemon host environment nor an
/// unpinned `.env` directory is consulted. Undeclared siblings read from the
/// store are zeroized before return; returned values zeroize on drop.
pub fn authorize_secret_delivery(
    vault: &dyn NodeVault,
    principal: &str,
    required_secrets: &[String],
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> std::result::Result<ryeos_engine::contracts::DeliveredSecretEnv, VaultReadError> {
    let (present, missing) =
        read_authorized_secret_values(vault, principal, required_secrets, project_authority)?;
    if !missing.is_empty() {
        return Err(VaultReadError::MissingSecrets {
            principal: principal.to_string(),
            names: missing,
        });
    }
    Ok(ryeos_engine::contracts::DeliveredSecretEnv::from_authorized(
        present,
    ))
}

/// Names-only preflight over the same authority as
/// [`authorize_secret_delivery`]. Values are read, zeroized and discarded.
pub fn check_secret_availability(
    vault: &dyn NodeVault,
    principal: &str,
    required_secrets: &[String],
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> std::result::Result<(), VaultReadError> {
    authorize_secret_delivery(vault, principal, required_secrets, project_authority).map(drop)
}

/// Report, per requested name and in request order, whether the admitted
/// environment authority would deliver it. Values are never returned.
pub fn report_secret_presence(
    vault: &dyn NodeVault,
    principal: &str,
    names: &[String],
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> std::result::Result<Vec<(String, bool)>, VaultReadError> {
    let (present, _missing) =
        read_authorized_secret_values(vault, principal, names, project_authority)?;
    Ok(names
        .iter()
        .map(|name| {
            let delivered = present.iter().any(|(candidate, _)| candidate == name);
            (name.clone(), delivered)
        })
        .collect())
}

/// Plain-value form retained for HMAC re-derivation, which delivers nothing
/// to a process. Delivering callers use [`authorize_secret_delivery`].
pub fn read_required_secrets_with_authority(
    vault: &dyn NodeVault,
    principal: &str,
    required_secrets: &[String],
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> std::result::Result<HashMap<String, String>, VaultReadError> {
    let (present, missing) =
        read_authorized_secret_values(vault, principal, required_secrets, project_authority)?;
    if !missing.is_empty() {
        return Err(VaultReadError::MissingSecrets {
            principal: principal.to_string(),
            names: missing,
        });
    }
    Ok(present
        .into_iter()
        .map(|(name, mut value)| (name, std::mem::take(&mut *value)))
        .collect())
}

type AuthorizedSecretValues = (Vec<(String, zeroize::Zeroizing<String>)>, Vec<String>);

/// Shared authority reader. Returns the requested values present under the
/// admitted authority, plus the requested names it could not supply.
fn read_authorized_secret_values(
    vault: &dyn NodeVault,
    principal: &str,
    required_secrets: &[String],
    project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
) -> std::result::Result<AuthorizedSecretValues, VaultReadError> {
    if required_secrets.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    for key in required_secrets {
        validate_operator_secret_name(key)?;
        crate::process::validate_spawn_secret_name(key)
            .map_err(|error| anyhow!("vault: invalid declared secret `{key}`: {error:#}"))?;
    }
    project_authority
        .validate()
        .map_err(VaultReadError::Internal)?;
    let authority = project_authority.environment();
    let name_authority = match authority {
        ryeos_state::objects::EnvironmentAuthority::None => None,
        ryeos_state::objects::EnvironmentAuthority::ProjectOverlay { name_authority, .. }
        | ryeos_state::objects::EnvironmentAuthority::Vault { name_authority, .. }
        | ryeos_state::objects::EnvironmentAuthority::Delegated { name_authority, .. } => {
            Some(name_authority)
        }
    };
    let unauthorized: Vec<String> = required_secrets
        .iter()
        .filter(|name| {
            !name_authority.is_some_and(|authority| authority.permits_declared(name.as_str()))
        })
        .cloned()
        .collect();
    if !unauthorized.is_empty() {
        return Err(VaultReadError::AuthorityViolation(format!(
            "declared secret name(s) exceed the admitted environment authority: [{}]",
            unauthorized.join(", ")
        )));
    }
    let requested = required_secrets.to_vec();
    let requested_set: HashSet<&str> = requested.iter().map(String::as_str).collect();
    let mut resolved = match authority {
        ryeos_state::objects::EnvironmentAuthority::None => HashMap::new(),
        ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
            project_authority_id,
            source_identity,
            include_operator_vault,
            ..
        } => {
            let root = project_authority.open_environment_root()?.ok_or_else(|| {
                VaultReadError::Internal(anyhow!(
                    "project environment authority has no authorized project root"
                ))
            })?;
            let stable_identity = match project_authority {
                ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
                    authored_project_identity,
                    ..
                } => authored_project_identity,
                ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                    stable_project_identity,
                    ..
                } => stable_project_identity,
                ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. } => {
                    return Err(VaultReadError::AuthorityViolation(
                        "projectless authority cannot carry a project environment overlay"
                            .to_string(),
                    ));
                }
            };
            let expected_authority_id = lillux::sha256_hex(
                format!(
                    "live-project\0{}\0{}",
                    stable_identity,
                    root.path().display()
                )
                .as_bytes(),
            );
            if project_authority_id != &expected_authority_id {
                return Err(VaultReadError::AuthorityViolation(
                    "project environment authority id does not match the descriptor-opened overlay root"
                        .to_string(),
                ));
            }
            let expected_source = format!("dotenv:{}", root.path().join(".env").display());
            if source_identity != &expected_source {
                return Err(VaultReadError::AuthorityViolation(format!(
                    "project environment source identity mismatch: expected {expected_source:?}, got {source_identity:?}"
                )));
            }
            let mut upstream = if *include_operator_vault {
                vault.read_all(principal)?
            } else {
                HashMap::new()
            };
            let wanted: HashSet<String> = requested
                .iter()
                .filter(|name| !upstream.contains_key(name.as_str()))
                .cloned()
                .collect();
            let overlay = match ryeos_vault::dotenv::read_dotenv_overlay_pinned(&root, &wanted) {
                Ok(overlay) => overlay,
                Err(error) => {
                    zeroize_secret_values(&mut upstream);
                    return Err(VaultReadError::Internal(anyhow!(
                        "dotenv overlay: {error:#}"
                    )));
                }
            };
            for (name, value) in overlay {
                upstream.entry(name).or_insert(value);
            }
            upstream
        }
        ryeos_state::objects::EnvironmentAuthority::Vault { namespace, .. } => {
            if namespace != "operator" {
                return Err(VaultReadError::Internal(anyhow!(
                    "unsupported vault namespace {namespace:?}; this node exposes only the explicit operator namespace"
                )));
            }
            vault.read_all(principal)?
        }
        ryeos_state::objects::EnvironmentAuthority::Delegated {
            provider, grant_id, ..
        } => {
            return Err(VaultReadError::Internal(anyhow!(
                "delegated environment provider {provider:?} grant {grant_id:?} is not installed"
            )));
        }
    };
    // Undeclared siblings never leave this function, not even as freed
    // heap bytes; survivors move into zeroizing ownership without a copy.
    let mut present = Vec::with_capacity(requested_set.len());
    for (name, mut value) in resolved.drain() {
        if requested_set.contains(name.as_str()) {
            present.push((name, zeroize::Zeroizing::new(value)));
        } else {
            zeroize::Zeroize::zeroize(&mut value);
        }
    }
    let missing: Vec<String> = required_secrets
        .iter()
        .filter(|name| !present.iter().any(|(candidate, _)| candidate == *name))
        .cloned()
        .collect();
    Ok((present, missing))
}

fn zeroize_secret_values(values: &mut HashMap<String, String>) {
    for value in values.values_mut() {
        zeroize::Zeroize::zeroize(value);
    }
}


/// Stub vault — used only when the daemon is constructed for a unit
/// test that doesn't want to depend on the operator's filesystem.
/// Always returns an empty map.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyVault;

impl NodeVault for EmptyVault {
    fn read_all_entries(
        &self,
        _principal: &str,
        _token: read_all_token::CrateOnly,
    ) -> Result<HashMap<String, String>> {
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
    fn relay_bootstrap_key(
        &self,
        access: &ExternalRelayBootstrapAccess,
        create: bool,
    ) -> Result<lillux::vault::VaultSecretKey> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        #[derive(serde::Serialize, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct KeyDocument {
            schema: u32,
            secret_key: String,
        }
        impl Drop for KeyDocument {
            fn drop(&mut self) {
                use zeroize::Zeroize as _;
                self.secret_key.zeroize();
            }
        }
        let decode = |raw: &str| -> Result<lillux::vault::VaultSecretKey> {
            let document: KeyDocument = serde_json::from_str(raw)?;
            anyhow::ensure!(
                document.schema == 1
                    && lillux::canonical_json(&serde_json::to_value(&document)?)? == raw,
                "protected relay artifact master key is not canonical"
            );
            let bytes = zeroize::Zeroizing::new(STANDARD.decode(&document.secret_key)?);
            anyhow::ensure!(
                bytes.len() == 32 && STANDARD.encode(&bytes) == document.secret_key,
                "protected relay key is not canonical"
            );
            let mut raw: [u8; 32] = bytes.as_slice().try_into()?;
            let key = lillux::vault::VaultSecretKey::from_bytes(raw);
            zeroize::Zeroize::zeroize(&mut raw);
            Ok(key)
        };
        if create {
            self.read_modify_write(|map| {
                if let Some(raw) = map.get(access.physical_key()) {
                    return decode(raw);
                }
                let key = lillux::vault::VaultSecretKey::generate();
                let mut key_bytes = key.to_bytes();
                let document = KeyDocument {
                    schema: 1,
                    secret_key: STANDARD.encode(key_bytes),
                };
                zeroize::Zeroize::zeroize(&mut key_bytes);
                let raw = lillux::canonical_json(&serde_json::to_value(&document)?)?;
                map.insert(access.physical_key().to_owned(), raw);
                Ok(key)
            })
        } else {
            let mut entries = self.read_all_internal()?;
            let raw = zeroize::Zeroizing::new(
                entries
                    .remove(access.physical_key())
                    .ok_or_else(|| anyhow!("protected original relay bootstrap key is absent"))?,
            );
            use zeroize::Zeroize as _;
            for value in entries.values_mut() {
                value.zeroize();
            }
            decode(raw.as_str())
        }
    }

    fn relay_bootstrap_directory(
        &self,
        access: &ExternalRelayBootstrapAccess,
        create: bool,
    ) -> Result<lillux::PinnedDirectory> {
        let parent_path = self
            .store_path
            .parent()
            .ok_or_else(|| anyhow!("vault store has no parent"))?;
        let parent = lillux::PinnedDirectory::open(parent_path)?
            .ok_or_else(|| anyhow!("protected vault directory is absent"))?;
        parent.require_owner_private_directory()?;
        let child = parent.open_child_directory(access.artifact_parent())?;
        let child = match child {
            Some(child) => child,
            None if create => parent.create_child(access.artifact_parent(), 0o700)?,
            None => bail!("protected original relay bootstrap directory is absent"),
        };
        child.require_owner_private_directory()?;
        Ok(child)
    }

    fn read_relay_bootstrap_artifact(
        &self,
        access: &ExternalRelayBootstrapAccess,
        key: &lillux::vault::VaultSecretKey,
    ) -> Result<ProtectedExternalRelayBootstrap> {
        let directory = self.relay_bootstrap_directory(access, false)?;
        let file = directory
            .open_pinned_regular(access.artifact_name(), false)?
            .ok_or_else(|| anyhow!("protected original relay bootstrap artifact is absent"))?;
        #[cfg(unix)]
        file.require_owner(unsafe { libc::geteuid() })?;
        let observed = file.observation()?;
        let encoded =
            file.read_stable_bounded(&observed, external_relay_bootstrap::MAX_ARTIFACT_BYTES)?;
        let envelope: lillux::vault::SealedEnvelope =
            toml::from_str(std::str::from_utf8(&encoded)?)?;
        let bytes = zeroize::Zeroizing::new(lillux::vault::open(key, &envelope)?);
        access.decode(bytes)
    }
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
        ryeos_vault::sealed::read_sealed_secrets(&self.store_path, secret_key).map_err(|error| {
            anyhow!(
                "vault: read sealed store {}: {error:#}",
                self.store_path.display()
            )
        })
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
    fn retain_external_relay_bootstrap(
        &self,
        access: &ExternalRelayBootstrapAccess,
        bootstrap: &ryeos_state::external_execution::transport::ExternalSupervisorBootstrap,
        signed_import: &ryeos_external_execution_contract::guest_import_authorization::SignedGuestImportAuthorization,
    ) -> Result<()> {
        let expected = access.canonical_document(bootstrap, signed_import)?;
        // The master key is one immutable internal vault value. Vault rewrap
        // carries it with the ordinary sealed store; artifact count does not
        // consume the vault entry or plaintext budgets per placement.
        let key = self.relay_bootstrap_key(access, true)?;
        let directory = self.relay_bootstrap_directory(access, true)?;
        if directory
            .open_pinned_regular(access.artifact_name(), false)?
            .is_none()
        {
            let envelope = lillux::vault::seal(&key.public_key(), &expected)?;
            let encoded = toml::to_string(&envelope)?;
            anyhow::ensure!(
                encoded.len() as u64 <= external_relay_bootstrap::MAX_ARTIFACT_BYTES,
                "protected relay bootstrap envelope exceeds its byte bound"
            );
            // A previously published original is never replaced. Any existing
            // artifact must be read and compared below before contact.
            directory.atomic_create_pinned_regular(
                access.artifact_name(),
                encoded.as_bytes(),
                0o600,
                crate::APP_TEMP_PURPOSE,
            )?;
        }
        let retained = self.read_relay_bootstrap_artifact(access, &key)?;
        anyhow::ensure!(
            access
                .canonical_document(retained.bootstrap(), retained.signed_import())?
                .as_slice()
                == expected.as_slice(),
            "protected original relay document changed before activation"
        );
        Ok(())
    }

    fn external_relay_bootstrap(
        &self,
        access: &ExternalRelayBootstrapAccess,
    ) -> Result<ProtectedExternalRelayBootstrap> {
        let key = self.relay_bootstrap_key(access, false)?;
        self.read_relay_bootstrap_artifact(access, &key)
    }

    fn external_connector_capability(
        &self,
        access: &ExternalConnectorCapabilityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        let mut entries = self.read_all_internal()?;
        let result = entries.remove(access.physical_key());
        use zeroize::Zeroize as _;
        for value in entries.values_mut() {
            value.zeroize();
        }
        result
            .map(zeroize::Zeroizing::new)
            .ok_or_else(|| anyhow!("protected external connector capability generation is absent"))
    }

    fn provision_external_connector_capability(
        &self,
        access: &ExternalConnectorCapabilityAccess,
        value: &str,
    ) -> Result<()> {
        validate_secret_value(value)?;
        access.validate_value(value)?;
        self.read_modify_write(|map| {
            if let Some(existing) = map.get(access.physical_key()) {
                if existing != value {
                    bail!("external connector capability generation is immutable");
                }
                return Ok(());
            }
            map.insert(access.physical_key().to_owned(), value.to_owned());
            Ok(())
        })
    }

    fn ensure_external_connector_capability(
        &self,
        access: &ExternalConnectorCapabilityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        self.read_modify_write(|map| {
            if let Some(existing) = map.get(access.physical_key()) {
                access.validate_value(existing)?;
                return Ok(zeroize::Zeroizing::new(existing.clone()));
            }
            let value = access.generate_value()?;
            access.validate_value(&value)?;
            map.insert(access.physical_key().to_owned(), value.clone());
            Ok(zeroize::Zeroizing::new(value))
        })
    }

    fn external_channel_authority(
        &self,
        access: &ExternalChannelAuthorityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        let mut entries = self.read_all_internal()?;
        let result = entries.remove(access.physical_key());
        use zeroize::Zeroize;
        for value in entries.values_mut() {
            value.zeroize();
        }
        result
            .map(zeroize::Zeroizing::new)
            .ok_or_else(|| anyhow!("protected external channel authority generation is absent"))
    }

    fn provision_external_channel_authority(
        &self,
        access: &ExternalChannelAuthorityAccess,
        value: &str,
    ) -> Result<()> {
        validate_secret_value(value)?;
        access.validate_value(value)?;
        self.read_modify_write(|map| {
            if let Some(existing) = map.get(access.physical_key()) {
                if existing != value {
                    bail!("external channel authority generation is immutable");
                }
                return Ok(());
            }
            map.insert(access.physical_key().to_owned(), value.to_owned());
            Ok(())
        })
    }

    fn ensure_external_channel_authority(
        &self,
        access: &ExternalChannelAuthorityAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        self.read_modify_write(|map| {
            if let Some(existing) = map.get(access.physical_key()) {
                access.validate_value(existing)?;
                return Ok(zeroize::Zeroizing::new(existing.clone()));
            }
            let value = access.generate_value()?;
            access.validate_value(&value)?;
            map.insert(access.physical_key().to_owned(), value.clone());
            Ok(zeroize::Zeroizing::new(value))
        })
    }

    fn placement_credential(
        &self,
        access: &PlacementCredentialAccess,
    ) -> Result<zeroize::Zeroizing<String>> {
        let mut entries = self.read_all_internal()?;
        let result = entries.remove(access.physical_key());
        // The decrypted store also contains other generations. Do not leave
        // copies of their values in this private accessor's temporary map.
        use zeroize::Zeroize;
        for value in entries.values_mut() {
            value.zeroize();
        }
        result
            .map(zeroize::Zeroizing::new)
            .ok_or_else(|| anyhow!("protected placement credential generation is absent"))
    }

    fn provision_placement_credential(
        &self,
        access: &PlacementCredentialAccess,
        value: &str,
    ) -> Result<()> {
        validate_secret_value(value)?;
        access.validate_value(value)?;
        self.read_modify_write(|map| {
            if let Some(existing) = map.get(access.physical_key()) {
                if existing != value {
                    bail!("placement credential generation is immutable");
                }
                return Ok(());
            }
            map.insert(access.physical_key().to_owned(), value.to_owned());
            Ok(())
        })
    }

    fn read_all_entries(
        &self,
        _principal: &str,
        _token: read_all_token::CrateOnly,
    ) -> Result<HashMap<String, String>> {
        Ok(self
            .read_all_internal()?
            .into_iter()
            .filter(|(key, _)| !is_internal_vault_key(key))
            .collect())
    }

    fn set_secret(&self, _principal: &str, name: &str, value: &str) -> Result<()> {
        validate_operator_secret_name(name)?;
        // Validate key name
        validate_key_name(name)?;
        validate_secret_value(value)?;
        self.read_modify_write(|map| {
            map.insert(name.to_string(), value.to_string());
            Ok(())
        })
    }

    fn list_keys(&self, _principal: &str) -> Result<Vec<String>> {
        let map = self.read_all_internal()?;
        let mut keys: Vec<String> = map
            .into_keys()
            .filter(|key| !is_internal_vault_key(key))
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
                validate_secret_value(value)?;
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
            VaultScope::OperatorEnv { principal } => Ok(read_named_secret(self, principal, key)?
                .map(|mut value| std::mem::take(&mut *value))),
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
/// when present and `Ok(None)` when absent. Generic launch preparation uses
/// this only after validating a symbolic requirement against the signed
/// runtime contract.
pub fn read_named_secret(
    vault: &dyn NodeVault,
    principal: &str,
    name: &str,
) -> Result<Option<zeroize::Zeroizing<String>>> {
    validate_operator_secret_name(name)?;
    let mut map = vault.read_all(principal)?;
    let selected = map.remove(name).map(zeroize::Zeroizing::new);
    zeroize_secret_values(&mut map);
    Ok(selected)
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
    struct FixedVault(HashMap<String, String>, std::sync::atomic::AtomicUsize);

    fn fixed_vault(entries: &[(&str, &str)]) -> FixedVault {
        FixedVault(
            entries
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
            std::sync::atomic::AtomicUsize::new(0),
        )
    }

    fn reads(vault: &FixedVault) -> usize {
        vault.1.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn vault_authority(
        namespace: &str,
        name_authority: ryeos_state::objects::EnvironmentNameAuthority,
    ) -> ryeos_state::objects::ExecutionProjectAuthority {
        ryeos_state::objects::ExecutionProjectAuthority::projectless(
            ryeos_state::objects::EnvironmentAuthority::Vault {
                namespace: namespace.to_string(),
                name_authority,
            },
        )
        .unwrap()
    }

    fn operator_authority() -> ryeos_state::objects::ExecutionProjectAuthority {
        vault_authority(
            "operator",
            ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
        )
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn authorized_delivery_returns_only_declared_values() {
        let vault = fixed_vault(&[
            ("OPENAI_API_KEY", "sk-1"),
            ("UNRELATED", "secret-not-declared"),
        ]);
        let delivered = authorize_secret_delivery(
            &vault,
            "op",
            &names(&["OPENAI_API_KEY"]),
            &operator_authority(),
        )
        .unwrap();
        assert_eq!(delivered.names().collect::<Vec<_>>(), ["OPENAI_API_KEY"]);
        assert_eq!(delivered.get("OPENAI_API_KEY"), Some("sk-1"));
        assert_eq!(delivered.get("UNRELATED"), None);
        assert!(!format!("{delivered:?}").contains("sk-1"));
        assert_eq!(reads(&vault), 1);
    }

    #[test]
    fn authorized_delivery_skips_the_store_when_nothing_is_declared() {
        let vault = fixed_vault(&[("OPENAI_API_KEY", "sk-1")]);
        let delivered =
            authorize_secret_delivery(&vault, "op", &[], &operator_authority()).unwrap();
        assert!(delivered.is_empty());
        assert_eq!(reads(&vault), 0);
    }

    #[test]
    fn authorized_delivery_lists_every_missing_name() {
        let vault = fixed_vault(&[("A", "present")]);
        let error = authorize_secret_delivery(
            &vault,
            "op",
            &names(&["A", "B", "C"]),
            &operator_authority(),
        )
        .unwrap_err();
        let VaultReadError::MissingSecrets { names, .. } = &error else {
            panic!("expected missing secrets, got {error}");
        };
        assert_eq!(names, &["B".to_string(), "C".to_string()]);
        assert!(!error.to_string().contains("present"));
    }

    #[test]
    fn authorized_delivery_refuses_names_outside_the_admitted_authority() {
        let vault = fixed_vault(&[("ALLOWED", "a"), ("OTHER", "b")]);
        let authority = vault_authority(
            "operator",
            ryeos_state::objects::EnvironmentNameAuthority::Exact {
                names: names(&["ALLOWED"]),
            },
        );
        let error =
            authorize_secret_delivery(&vault, "op", &names(&["ALLOWED", "OTHER"]), &authority)
                .unwrap_err();
        assert!(matches!(error, VaultReadError::AuthorityViolation(_)), "{error}");
        assert!(error.to_string().contains("OTHER"));
        assert_eq!(reads(&vault), 0, "an unauthorized name must not read the store");
    }

    #[test]
    fn authorized_delivery_refuses_non_operator_namespace_and_delegation() {
        let vault = fixed_vault(&[("TOKEN", "t")]);
        let foreign = vault_authority(
            "tenant",
            ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
        );
        assert!(authorize_secret_delivery(&vault, "op", &names(&["TOKEN"]), &foreign).is_err());
        let delegated = ryeos_state::objects::ExecutionProjectAuthority::projectless(
            ryeos_state::objects::EnvironmentAuthority::Delegated {
                provider: "provider".to_string(),
                grant_id: "grant".to_string(),
                name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
            },
        )
        .unwrap();
        assert!(authorize_secret_delivery(&vault, "op", &names(&["TOKEN"]), &delegated).is_err());
        assert_eq!(reads(&vault), 0);
    }

    #[test]
    fn authorized_delivery_refuses_an_overlay_identity_mismatch() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".env"), "TOKEN=from-overlay\n").unwrap();
        use crate::execution_policy::ExecutionPolicyResolution as _;
        let mut authority = crate::execution_policy::ExecutionPolicy::local_live(
            crate::execution_policy::ExecutionResponse::Wait,
        )
        .resolve_live_project_authority(
            project.path(),
            ryeos_state::objects::LiveFilesystemConfinement::UnconfinedHost,
            Vec::new(),
        )
        .unwrap();
        let ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
            environment:
                ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                    source_identity, ..
                },
            ..
        } = &mut authority
        else {
            panic!("local live policy must admit a project overlay");
        };
        *source_identity = "dotenv:/substituted/.env".to_string();
        let vault = fixed_vault(&[]);
        let error = authorize_secret_delivery(&vault, "op", &names(&["TOKEN"]), &authority)
            .unwrap_err();
        assert!(matches!(error, VaultReadError::AuthorityViolation(_)), "{error}");
        assert!(!error.to_string().contains("from-overlay"));
    }

    #[test]
    fn host_environment_is_not_a_secret_source() {
        // Any variable the test process already has qualifies as host env.
        let (name, _) = std::env::vars()
            .find(|(name, _)| validate_operator_secret_name(name).is_ok()
                && crate::process::validate_spawn_secret_name(name).is_ok())
            .unwrap_or_else(|| ("RYE_TEST_ABSENT_HOST_SECRET".to_string(), String::new()));
        let vault = fixed_vault(&[]);
        let error = authorize_secret_delivery(
            &vault,
            "op",
            std::slice::from_ref(&name),
            &operator_authority(),
        )
        .unwrap_err();
        assert!(matches!(error, VaultReadError::MissingSecrets { .. }), "{error}");
    }
    impl NodeVault for FixedVault {
        fn read_all_entries(
            &self,
            _principal: &str,
            _token: read_all_token::CrateOnly,
        ) -> Result<HashMap<String, String>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    fn sealed_vault_store_refuses_protected_subprocess_env_names() {
        let tmp = tempfile::tempdir().unwrap();
        let v = SealedEnvelopeVault::new(
            tmp.path().join("store.enc"),
            lillux::vault::VaultSecretKey::generate(),
        );
        for name in ["LD_PRELOAD", "PATH"] {
            assert!(v.set_secret("op", name, "value").is_err(), "{name} was stored");
        }
        assert!(v.list_keys("op").unwrap().is_empty());
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


    // ── .env overlay layering ────────────────────────────────────















    // ── read_named_secret tests ──────────────────────────────────

    #[test]
    fn read_named_secret_present() {
        let v = fixed_vault(&[("ZEN_API_KEY", "sk-zen"), ("OTHER", "other-val")]);
        let result = read_named_secret(&v, "op", "ZEN_API_KEY").unwrap();
        assert_eq!(result.as_deref().map(String::as_str), Some("sk-zen"));
    }

    #[test]
    fn read_named_secret_absent() {
        let v = fixed_vault(&[("OTHER", "other-val")]);
        let result = read_named_secret(&v, "op", "ZEN_API_KEY").unwrap();
        assert!(result.is_none());
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
            read_named_secret(&v, "op", "OPENAI_API_KEY")
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("sk-op")
        );
        assert_eq!(
            read_named_secret(&v, "op", "google_account_123")
                .unwrap()
                .as_deref()
                .map(String::as_str),
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

        let err = authorize_secret_delivery(
            &v,
            "op",
            std::slice::from_ref(&name),
            &operator_authority(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("reserved internal runtime vault prefix"));
        let err = read_named_secret(&v, "op", &name).unwrap_err();
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
