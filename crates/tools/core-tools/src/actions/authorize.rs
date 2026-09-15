//! Authorize an HTTP client to call the daemon's authenticated endpoints.
//!
//! Writes a node-signed authorized-key TOML to
//! `<app_root>/.ai/node/auth/authorized_keys/<fp>.toml`.
//!
//! The daemon's auth loader reads these files at startup (and on hot-reload).
//! Each file must be signed by the node identity key.
//!
//! Delegates to the canonical `ryeos_app::identity::write_authorized_key_toml`
//! so there is exactly one TOML emitter.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use lillux::crypto::VerifyingKey;
use rand::RngCore;

use crate::actions::hosted_policy::load_hosted_policy;

const DEFAULT_ADMISSION_TOKEN_TTL_SECS: u64 = 600;

/// Shared input for node-owned grant reconciliation. The target app root and
/// signing authority come from the host entrypoint, never from this payload.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeClientRequest {
    pub public_key: String,
    pub scopes: String,
    #[serde(default = "default_authorize_client_label")]
    pub label: String,
    #[serde(default)]
    pub merge_scopes: bool,
    #[serde(default)]
    pub origin_site_id: Option<String>,
    /// Bind this key as the authenticated identity of a remote RyeOS node.
    /// Mutually exclusive with `origin_site_id`, which selects a forwarded
    /// remote operator.
    #[serde(default)]
    pub remote_node_origin_site_id: Option<String>,
    #[serde(default)]
    pub allow_semantic_conversion: bool,
}

fn default_authorize_client_label() -> String {
    "cli-authorized".into()
}

impl AuthorizeClientRequest {
    pub fn into_params(self, app_root: PathBuf) -> Result<AuthorizeClientParams> {
        if self.origin_site_id.is_some() && self.remote_node_origin_site_id.is_some() {
            bail!("origin_site_id and remote_node_origin_site_id are mutually exclusive");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.public_key)
            .context("invalid base64 public key")?;
        let public_key = VerifyingKey::from_bytes(
            bytes
                .as_slice()
                .try_into()
                .context("public key must be 32 bytes (ed25519)")?,
        )
        .context("invalid ed25519 public key")?;
        let scopes: Vec<String> = self
            .scopes
            .split(',')
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect();
        if scopes.is_empty() {
            bail!("scopes must not be empty");
        }
        for scope in &scopes {
            ryeos_runtime::authorizer::validate_scope_pattern(scope)
                .map_err(|error| anyhow::anyhow!("invalid scope: {error}"))?;
        }
        let subject = match (self.origin_site_id, self.remote_node_origin_site_id) {
            (Some(origin_site_id), None) => {
                AuthorizeClientSubject::RemoteOperator { origin_site_id }
            }
            (None, Some(origin_site_id)) => AuthorizeClientSubject::RemoteNode { origin_site_id },
            (None, None) => AuthorizeClientSubject::LocalClient,
            (Some(_), Some(_)) => unreachable!("mutual exclusion checked above"),
        };
        Ok(AuthorizeClientParams {
            app_root,
            public_key,
            scopes,
            label: self.label,
            allow_wildcard: false,
            merge: self.merge_scopes,
            subject,
            allow_semantic_conversion: self.allow_semantic_conversion,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizeClientSubject {
    LocalClient,
    RemoteNode { origin_site_id: String },
    RemoteOperator { origin_site_id: String },
}

/// Parameters for the authorize-client action.
pub struct AuthorizeClientParams {
    /// App root directory (contains `.ai/node/identity/`).
    pub app_root: PathBuf,
    /// Client public key as raw 32-byte Ed25519 verifying key.
    pub public_key: VerifyingKey,
    /// Scopes to grant (e.g. `["remote.admin", "bundle.install"]`).
    /// Pass `["*"]` only with `allow_wildcard: true`.
    pub scopes: Vec<String>,
    /// Human-readable label for the key file.
    pub label: String,
    /// Allow wildcard `"*"` in scopes. Should only be `true` for
    /// operator bootstrap.
    pub allow_wildcard: bool,
    /// When `true`, union `scopes` with any scopes already present in the
    /// existing authorized-key file for this fingerprint instead of
    /// replacing them. Mirrors the `--merge-scopes` CLI flag. Without it,
    /// the write replaces the scope set (and any dropped scope is reported
    /// in `AuthorizeClientResult::dropped_scopes`).
    pub merge: bool,
    /// Exact semantic class and site binding for the grant.
    pub subject: AuthorizeClientSubject,
    /// Explicitly authorize changing an incumbent grant's principal class or
    /// origin constraint. Operationally this is valid only with the daemon
    /// stopped; ordinary scope updates leave it false.
    pub allow_semantic_conversion: bool,
}

/// Result of a successful authorize-client run.
#[derive(Debug, serde::Serialize)]
pub struct AuthorizeClientResult {
    /// Fingerprint of the authorized key.
    pub fingerprint: String,
    /// Path of the written TOML file.
    pub path: PathBuf,
    /// Scopes that existed on the prior authorized-key file but are NOT in
    /// the scope set just written. Empty when the file was new or when
    /// `merge` preserved everything. The caller should warn loudly if this
    /// is non-empty — it means an existing grant was narrowed.
    pub dropped_scopes: Vec<String>,
    /// Whether existing scopes were merged into the written set.
    pub merged: bool,
    /// Origin site constraint for a remote grant.
    pub origin_site_id: Option<String>,
    /// Exact incumbent semantic class observed under the publication lock.
    pub previous_principal_class: Option<String>,
    /// Exact incumbent origin constraint observed under the publication lock.
    pub previous_origin_site_id: Option<String>,
    /// Semantic class written by this operation.
    pub principal_class: String,
}

/// Reconcile a requested scope set against the scopes already on disk.
///
/// Returns `(final_scopes, dropped_scopes)`. With `merge`, the result is
/// `existing ∪ requested` (order-preserving) and nothing is dropped. Without
/// `merge`, the result is exactly `requested` and `dropped` lists the existing
/// scopes that are not being re-granted.
#[cfg(test)]
fn reconcile_scopes(
    existing: &[String],
    requested: &[String],
    merge: bool,
) -> (Vec<String>, Vec<String>) {
    if merge {
        let mut final_scopes = existing.to_vec();
        for s in requested {
            if !final_scopes.contains(s) {
                final_scopes.push(s.clone());
            }
        }
        (final_scopes, Vec::new())
    } else {
        let dropped = existing
            .iter()
            .filter(|s| !requested.contains(s))
            .cloned()
            .collect();
        (requested.to_vec(), dropped)
    }
}

pub struct MintAdmissionTokenParams {
    /// App root directory for the target node.
    pub app_root: PathBuf,
    /// Capabilities this one-time token is allowed to grant.
    pub scopes: Vec<String>,
    /// Optional default label for the eventual authorized-key entry.
    pub label: Option<String>,
    /// Token lifetime in seconds.
    pub ttl_secs: u64,
}

/// Signed-service input for minting one target-local admission token.
///
/// The service composition root supplies the selected node root and its
/// already-compiled hosted policy. A caller never supplies either authority.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintAdmissionTokenRequest {
    pub scopes: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "default_admission_token_ttl_secs")]
    pub ttl_secs: u64,
}

fn default_admission_token_ttl_secs() -> u64 {
    DEFAULT_ADMISSION_TOKEN_TTL_SECS
}

impl MintAdmissionTokenRequest {
    pub fn into_params(self, app_root: PathBuf) -> Result<MintAdmissionTokenParams> {
        let scopes = self
            .scopes
            .split(',')
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(MintAdmissionTokenParams {
            app_root,
            scopes,
            label: self.label,
            ttl_secs: self.ttl_secs,
        })
    }
}

#[derive(serde::Serialize, Debug)]
pub struct MintAdmissionTokenResult {
    /// One-time bearer token. Show once to the local node being admitted.
    pub token: String,
    /// SHA-256 hash of `token`, used as the token file name.
    pub token_hash: String,
    /// Path of the target-node-local token file.
    pub path: PathBuf,
    /// Unix timestamp when the token was minted.
    pub issued_at_unix: u64,
    /// Original requested token lifetime in seconds.
    pub ttl_secs: u64,
    /// Unix expiry timestamp.
    pub expires_at_unix: u64,
    /// Scopes this token may grant.
    pub scopes: Vec<String>,
    /// Optional default label stored in the token file.
    pub label: Option<String>,
}

#[derive(serde::Serialize)]
struct AdmissionTokenFile<'a> {
    version: u32,
    token_hash: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    scopes: &'a [String],
    issued_at_unix: u64,
    ttl_secs: u64,
    expires_at_unix: u64,
}

/// Authorize a client by writing a node-signed authorized-key TOML.
///
/// Reconciles an existing fingerprint only within the explicitly selected
/// same-class or stopped-node semantic-transition contract.
///
/// Delegates to the canonical writer in `ryeos_app::identity` so the
/// TOML format is identical to what the daemon's own handler produces.
pub fn run_authorize_client(params: AuthorizeClientParams) -> Result<AuthorizeClientResult> {
    // Explicit pre-node bootstrap entry only. A confined Tool must never
    // reopen node private state; normal CLI use goes through the node-owned
    // identity/authorize-client service and its retained identity instead.
    let _stopped_node_lock = acquire_semantic_conversion_lock(&params)?;
    let root = ryeos_engine::roots::RuntimeRoot::new(params.app_root.clone());
    let node_identity = ryeos_app::identity::NodeIdentity::load(&root.node_signing_key_path())?;
    run_authorize_client_with_authority(
        params,
        &node_identity,
        &root.authorized_keys_dir(),
        _stopped_node_lock.as_ref(),
    )
}

/// Reuse the canonical grant writer with the selected node's retained
/// authority. This is a local operator operation, not a worker permission.
pub fn run_authorize_client_with_authority(
    params: AuthorizeClientParams,
    node_identity: &ryeos_app::identity::NodeIdentity,
    auth_dir: &std::path::Path,
    stopped_node_authority: Option<&ryeos_app::state_lock::StateLock>,
) -> Result<AuthorizeClientResult> {
    if params.allow_semantic_conversion {
        stopped_node_authority
            .context("semantic authorized-key conversion requires stopped-node authority")?
            .ensure_protects_app_root(&params.app_root)?;
    }
    reconcile_client_grant(params, node_identity, auth_dir)
}

fn acquire_semantic_conversion_lock(
    params: &AuthorizeClientParams,
) -> Result<Option<ryeos_app::state_lock::StateLock>> {
    // Principal-class and origin changes alter the meaning of an existing
    // fingerprint. Prove stopped-node ownership and retain it through the
    // read/verify/sign/publish transaction instead of treating the CLI flag
    // as sufficient authority on its own. Ordinary same-class provisioning
    // remains usable for bootstrap and release tooling while the daemon runs.
    params
        .allow_semantic_conversion
        .then(|| {
            let lock_path = ryeos_app::state_lock::default_lock_path(&params.app_root);
            ryeos_app::state_lock::StateLock::acquire(&lock_path).with_context(
                || "semantic authorized-key conversion requires stopped-node authority",
            )
        })
        .transpose()
}

fn reconcile_client_grant(
    params: AuthorizeClientParams,
    node_identity: &ryeos_app::identity::NodeIdentity,
    auth_dir: &std::path::Path,
) -> Result<AuthorizeClientResult> {
    if params.subject != AuthorizeClientSubject::LocalClient && params.allow_wildcard {
        bail!("remote grants require exact, non-wildcard scopes");
    }
    let fp = lillux::crypto::fingerprint(&params.public_key);
    let key_b64 = base64::engine::general_purpose::STANDARD.encode(params.public_key.as_bytes());

    let now = lillux::time::iso8601_now();

    let wildcard = if params.allow_wildcard {
        ryeos_app::identity::WildcardPolicy::AllowBootstrap
    } else {
        ryeos_app::identity::WildcardPolicy::Reject
    };

    // Verified load, scope reconciliation, signing, and conditional
    // publication share one descriptor-pinned directory lock. A concurrent
    // merge can therefore never silently lose scopes.
    let identity_subject = match &params.subject {
        AuthorizeClientSubject::LocalClient => {
            ryeos_app::identity::AuthorizedKeySubject::LocalClient
        }
        AuthorizeClientSubject::RemoteNode { origin_site_id } => {
            ryeos_app::identity::AuthorizedKeySubject::RemoteNode { origin_site_id }
        }
        AuthorizeClientSubject::RemoteOperator { origin_site_id } => {
            ryeos_app::identity::AuthorizedKeySubject::RemoteOperator { origin_site_id }
        }
    };
    let (path, dropped_scopes, transition) =
        ryeos_app::identity::reconcile_authorized_key_toml_scopes_for_subject(
            auth_dir,
            &fp,
            &key_b64,
            &params.scopes,
            &params.label,
            "cli-authorize-key",
            &now,
            node_identity,
            wildcard,
            params.merge,
            identity_subject,
            params.allow_semantic_conversion,
        )
        .context("failed to write authorized-key TOML")?;

    Ok(AuthorizeClientResult {
        fingerprint: fp,
        path,
        dropped_scopes,
        merged: params.merge,
        origin_site_id: transition.origin_site_id.clone(),
        previous_principal_class: transition
            .previous_principal_class
            .map(|class| class.as_str().to_string()),
        previous_origin_site_id: transition.previous_origin_site_id,
        principal_class: transition.principal_class.as_str().to_string(),
    })
}

pub fn run_mint_admission_token(
    params: MintAdmissionTokenParams,
) -> Result<MintAdmissionTokenResult> {
    if params.ttl_secs == 0 {
        bail!("ttl_secs must be greater than zero");
    }
    let policy = load_hosted_policy(&params.app_root)?;
    mint_admission_token_with_policy(params, &policy.policy, &policy.source_file)
}

/// Mint through the daemon-owned service using its already-loaded, exact
/// policy generation. This deliberately avoids reopening node-private policy
/// state from a confined subprocess Tool.
pub fn mint_admission_token_with_policy(
    params: MintAdmissionTokenParams,
    policy: &ryeos_app::node_policy::sections::hosted::HostedNodePolicy,
    policy_source: &std::path::Path,
) -> Result<MintAdmissionTokenResult> {
    if params.ttl_secs == 0 {
        bail!("ttl_secs must be greater than zero");
    }
    if !policy.admission_enabled {
        bail!(
            "hosted-node admission is disabled by policy from {}",
            policy_source.display()
        );
    }
    let maximum_token_ttl_secs = policy
        .admission_token_ttl_secs
        .context("enabled hosted-node admission policy is missing its bounded token TTL")?;
    if params.ttl_secs > maximum_token_ttl_secs {
        bail!(
            "ttl_secs {} exceeds hosted-node policy maximum {} from {}",
            params.ttl_secs,
            maximum_token_ttl_secs,
            policy_source.display()
        );
    }

    let mut scopes = params
        .scopes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() {
        bail!("scopes must not be empty");
    }
    if scopes.iter().any(|scope| scope.contains('*')) {
        bail!("wildcard scopes are not allowed in admission tokens");
    }
    for scope in &scopes {
        ryeos_runtime::authorizer::validate_scope_pattern(scope)
            .map_err(|e| anyhow::anyhow!("invalid scope: {e}"))?;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expires_at_unix = now
        .checked_add(params.ttl_secs)
        .ok_or_else(|| anyhow::anyhow!("ttl_secs overflows unix timestamp"))?;

    let mut token_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
    let token_hash = lillux::cas::sha256_hex(token.as_bytes());
    let label = params.label.clone();
    let token_dir = params
        .app_root
        .join(".ai")
        .join("node")
        .join("admission")
        .join("tokens");
    std::fs::create_dir_all(&token_dir).with_context(|| {
        format!(
            "failed to create admission token dir {}",
            token_dir.display()
        )
    })?;
    let path = token_dir.join(format!("{token_hash}.toml"));

    let doc = toml::to_string(&AdmissionTokenFile {
        version: 1,
        token_hash: &token_hash,
        label: label.as_deref(),
        scopes: &scopes,
        issued_at_unix: now,
        ttl_secs: params.ttl_secs,
        expires_at_unix,
    })?;
    let token_dir = lillux::PinnedDirectory::open(&token_dir)?
        .ok_or_else(|| anyhow::anyhow!("admission token directory is unavailable"))?;
    token_dir
        .atomic_write_if_same(
            path.file_name().expect("token path has a file name"),
            None,
            doc.as_bytes(),
            0o600,
        )
        .with_context(|| format!("failed to install admission token file {}", path.display()))?;
    token_dir.ensure_path_binding()?;

    Ok(MintAdmissionTokenResult {
        token,
        token_hash,
        path,
        issued_at_unix: now,
        ttl_secs: params.ttl_secs,
        expires_at_unix,
        scopes,
        label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::EncodePrivateKey;
    use rand::rngs::OsRng;
    struct HostedPolicyFixture {
        _user: std::path::PathBuf,
        key: lillux::crypto::SigningKey,
    }

    impl HostedPolicyFixture {
        fn new(root: &std::path::Path) -> Self {
            let user = root.join("user");
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = lillux::crypto::SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            write_node_bootstrap(root, &trust_dir, &key);
            Self { _user: user, key }
        }
    }

    fn write_hosted_policy(
        app_root: &std::path::Path,
        admission_enabled: bool,
        token_ttl_secs: u64,
        key: &lillux::crypto::SigningKey,
    ) {
        let path = app_root.join(".ai/node/policies/hosted.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ttl = admission_enabled
            .then(|| format!("admission_token_ttl_secs: {token_ttl_secs}\n"))
            .unwrap_or_default();
        let body = format!(
            r#"
schema: 1
admission_enabled: {admission_enabled}
{ttl}allow_loopback_http: true
"#
        );
        std::fs::write(path, lillux::signature::sign_content(&body, key, "#", None)).unwrap();
    }

    fn write_node_bootstrap(
        app_root: &std::path::Path,
        trust_dir: &std::path::Path,
        fallback_key: &lillux::crypto::SigningKey,
    ) {
        let app_trust_dir = app_root.join(".ai/config/keys/trusted");
        std::fs::create_dir_all(&app_trust_dir).unwrap();
        ryeos_engine::trust::pin_key(&fallback_key.verifying_key(), "test", &app_trust_dir, None)
            .unwrap();

        let identity_dir = app_root.join(".ai/node/identity");
        std::fs::create_dir_all(&identity_dir).unwrap();
        let identity_path = identity_dir.join("private_key.pem");
        let node_identity = if identity_path.exists() {
            ryeos_app::identity::NodeIdentity::load(&identity_path).unwrap()
        } else {
            std::fs::write(
                &identity_path,
                fallback_key
                    .to_pkcs8_pem(Default::default())
                    .unwrap()
                    .as_bytes(),
            )
            .unwrap();
            ryeos_app::identity::NodeIdentity::load(&identity_path).unwrap()
        };
        ryeos_engine::trust::pin_key(node_identity.verifying_key(), "node", trust_dir, None)
            .unwrap();
        ryeos_engine::trust::pin_key(node_identity.verifying_key(), "node", &app_trust_dir, None)
            .unwrap();

        crate::actions::hosted_policy::write_required_non_hosted_test_policies(
            app_root,
            node_identity.signing_key(),
        );
    }

    #[test]
    fn reconcile_scopes_replace_reports_dropped() {
        let existing = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let requested = vec!["a".to_string(), "d".to_string()];
        let (final_scopes, dropped) = reconcile_scopes(&existing, &requested, false);
        assert_eq!(final_scopes, vec!["a".to_string(), "d".to_string()]);
        // b and c existed but were not re-granted.
        assert_eq!(dropped, vec!["b".to_string(), "c".to_string()]);
    }

    #[test]
    fn reconcile_scopes_merge_unions_and_drops_nothing() {
        let existing = vec!["a".to_string(), "b".to_string()];
        let requested = vec!["b".to_string(), "c".to_string()];
        let (final_scopes, dropped) = reconcile_scopes(&existing, &requested, true);
        // existing order preserved, new appended, no duplicates.
        assert_eq!(
            final_scopes,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(dropped.is_empty());
    }

    #[test]
    fn reconcile_scopes_new_file_no_drops() {
        let (final_scopes, dropped) = reconcile_scopes(&[], &["x".to_string()], false);
        assert_eq!(final_scopes, vec!["x".to_string()]);
        assert!(dropped.is_empty());
    }

    #[test]
    fn admission_token_request_has_no_node_root_input() {
        let request: MintAdmissionTokenRequest = serde_json::from_value(serde_json::json!({
            "scopes": "ryeos.attest.request.forwarded-operator"
        }))
        .expect("signed service input must decode");
        let params = request.into_params(PathBuf::from("/node")).unwrap();
        assert_eq!(params.app_root, PathBuf::from("/node"));
        assert_eq!(params.ttl_secs, DEFAULT_ADMISSION_TOKEN_TTL_SECS);
        assert_eq!(
            params.scopes,
            vec!["ryeos.attest.request.forwarded-operator"]
        );

        let err = serde_json::from_value::<MintAdmissionTokenRequest>(serde_json::json!({
            "scopes": "ryeos.attest.request.forwarded-operator",
            "system_space_dir": "/attacker-selected-node"
        }))
        .expect_err("service input must not select a node root");
        assert!(err.to_string().contains("system_space_dir"));
    }

    #[test]
    fn authorize_client_can_emit_exact_scope_remote_operator_grant() {
        let tmp = tempfile::tempdir().unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        let client = lillux::crypto::SigningKey::generate(&mut OsRng).verifying_key();
        let result = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.execute.service.remote/run".to_owned()],
            label: "forwarded operator".to_owned(),
            allow_wildcard: false,
            merge: false,
            subject: AuthorizeClientSubject::RemoteOperator {
                origin_site_id: "site:source".to_owned(),
            },
            allow_semantic_conversion: false,
        })
        .unwrap();

        assert_eq!(result.origin_site_id.as_deref(), Some("site:source"));
        let signed = std::fs::read_to_string(result.path).unwrap();
        assert!(signed.contains("principal_class = \"remote_operator\""));
        assert!(signed.contains("origin_site_id = \"site:source\""));
    }

    #[test]
    fn authorize_client_reports_and_requires_explicit_semantic_conversion() {
        let tmp = tempfile::tempdir().unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        let client = lillux::crypto::SigningKey::generate(&mut OsRng).verifying_key();
        let local = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.execute.service.remote/run".to_owned()],
            label: "operator".to_owned(),
            allow_wildcard: false,
            merge: false,
            subject: AuthorizeClientSubject::LocalClient,
            allow_semantic_conversion: false,
        })
        .unwrap();
        assert_eq!(local.principal_class, "local_client");
        assert_eq!(local.previous_principal_class, None);

        let denied = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.execute.service.remote/run".to_owned()],
            label: "operator".to_owned(),
            allow_wildcard: false,
            merge: false,
            subject: AuthorizeClientSubject::RemoteOperator {
                origin_site_id: "site:source".to_owned(),
            },
            allow_semantic_conversion: false,
        })
        .expect_err("class conversion must require explicit authorization");
        assert!(
            format!("{denied:#}").contains("semantic-conversion"),
            "got: {denied:#}"
        );

        let converted = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.execute.service.remote/run".to_owned()],
            label: "operator".to_owned(),
            allow_wildcard: false,
            merge: false,
            subject: AuthorizeClientSubject::RemoteOperator {
                origin_site_id: "site:source".to_owned(),
            },
            allow_semantic_conversion: true,
        })
        .unwrap();
        assert_eq!(
            converted.previous_principal_class.as_deref(),
            Some("local_client")
        );
        assert_eq!(converted.previous_origin_site_id, None);
        assert_eq!(converted.principal_class, "remote_operator");
        assert_eq!(converted.origin_site_id.as_deref(), Some("site:source"));

        let lock_path = ryeos_app::state_lock::default_lock_path(tmp.path());
        let _live_daemon_lock = ryeos_app::state_lock::StateLock::acquire(&lock_path).unwrap();
        let while_live = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.execute.service.remote/run".to_owned()],
            label: "operator".to_owned(),
            allow_wildcard: false,
            merge: false,
            subject: AuthorizeClientSubject::LocalClient,
            allow_semantic_conversion: true,
        })
        .expect_err("semantic conversion must prove stopped-node ownership");
        assert!(
            format!("{while_live:#}").contains("stopped-node authority"),
            "got: {while_live:#}"
        );
    }

    #[test]
    fn authorize_client_maintains_remote_node_subject_without_conversion() {
        let tmp = tempfile::tempdir().unwrap();
        let _fixture = HostedPolicyFixture::new(tmp.path());
        let client = lillux::crypto::SigningKey::generate(&mut OsRng).verifying_key();
        let subject = AuthorizeClientSubject::RemoteNode {
            origin_site_id: "site:source".to_owned(),
        };
        let first = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.attest.request.forwarded-operator".to_owned()],
            label: "remote node".to_owned(),
            allow_wildcard: false,
            merge: false,
            subject: subject.clone(),
            allow_semantic_conversion: false,
        })
        .unwrap();
        assert_eq!(first.principal_class, "remote_node");

        let maintained = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.execute.service.objects/has".to_owned()],
            label: "remote node".to_owned(),
            allow_wildcard: false,
            merge: true,
            subject,
            allow_semantic_conversion: false,
        })
        .unwrap();
        assert_eq!(
            maintained.previous_principal_class.as_deref(),
            Some("remote_node")
        );
        assert_eq!(maintained.principal_class, "remote_node");
        assert_eq!(maintained.origin_site_id.as_deref(), Some("site:source"));
        let signed = std::fs::read_to_string(maintained.path).unwrap();
        assert!(signed.contains("principal_class = \"remote_node\""));
        assert!(signed.contains("ryeos.attest.request.forwarded-operator"));
        assert!(signed.contains("ryeos.execute.service.objects/has"));

        let wrong_origin = run_authorize_client(AuthorizeClientParams {
            app_root: tmp.path().to_path_buf(),
            public_key: client,
            scopes: vec!["ryeos.execute.service.objects/get".to_owned()],
            label: "remote node".to_owned(),
            allow_wildcard: false,
            merge: true,
            subject: AuthorizeClientSubject::RemoteNode {
                origin_site_id: "site:different".to_owned(),
            },
            allow_semantic_conversion: false,
        })
        .expect_err("scope merge must not conceal a remote-node origin change");
        assert!(format!("{wrong_origin:#}").contains("cannot merge scopes"));
    }

    #[test]
    fn mint_admission_token_rejects_ttl_above_hosted_policy() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), true, 60, &fixture.key);

        let err = match run_mint_admission_token(MintAdmissionTokenParams {
            app_root: tmp.path().to_path_buf(),
            scopes: vec!["ryeos.execute.service.threads".into()],
            label: None,
            ttl_secs: 600,
        }) {
            Ok(_) => panic!("minting should reject TTL above hosted policy"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("hosted-node policy maximum"),
            "got: {err:#}"
        );
    }

    #[test]
    fn mint_admission_token_refuses_when_hosted_admission_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = HostedPolicyFixture::new(tmp.path());
        write_hosted_policy(tmp.path(), false, 60, &fixture.key);

        let err = run_mint_admission_token(MintAdmissionTokenParams {
            app_root: tmp.path().to_path_buf(),
            scopes: vec!["ryeos.execute.service.threads".into()],
            label: None,
            ttl_secs: 60,
        })
        .expect_err("explicitly disabled hosted admission must block token minting");

        assert!(
            format!("{err:#}").contains("hosted-node admission is disabled"),
            "got: {err:#}"
        );
    }
}
