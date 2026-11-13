use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use base64::Engine;
use lillux::crypto::{Signature, Verifier, VerifyingKey};

use ryeos_app::state::AppState;
use ryeos_node_runtime::identity::{
    AuthorizedKeyError, AuthorizedKeyPrincipalClass, GrantExpiry, NodeIdentity,
};
use ryeos_node_runtime::remote_delegation::DelegationError;

const TIMESTAMP_MAX_AGE_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Principal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Principal {
    pub fingerprint: String,
    pub scopes: Vec<String>,
    pub owner: String,
    pub principal_class: AuthorizedKeyPrincipalClass,
    pub authenticated_site_id: Option<String>,
    pub grant_authority: ryeos_engine::principal_contract::AuthenticatedGrantAuthority,
    /// `ryeos-request-v1` content hash of the signed request this principal
    /// was verified from.
    pub request_digest: String,
    /// Site of the node that forwarded this request under the principal's
    /// delegation, when it did. The principal is then the delegator.
    pub delegated_via: Option<String>,
}

// ---------------------------------------------------------------------------
// Replay guard
// ---------------------------------------------------------------------------

/// Ceiling on remembered nonces per fingerprint. Legitimate callers sit
/// far below this within the replay window; hitting it means abuse or a
/// runaway client, and requests are rejected (fail closed) rather than
/// evicting older nonces, which would re-open the replay window.
const MAX_NONCES_PER_FINGERPRINT: usize = 4096;

struct ReplayGuard {
    seen: HashMap<String, Vec<(String, Instant)>>,
    max_age: Duration,
}

impl ReplayGuard {
    fn new() -> Self {
        Self {
            seen: HashMap::new(),
            max_age: Duration::from_secs(TIMESTAMP_MAX_AGE_SECS + 60),
        }
    }

    fn check_and_record(&mut self, fingerprint: &str, nonce: &str) -> bool {
        let now = Instant::now();
        // Drop fingerprints whose nonces have all expired so the map
        // doesn't grow with every principal ever seen.
        self.seen.retain(|_, entries| {
            entries.retain(|(_, ts)| now.duration_since(*ts) < self.max_age);
            !entries.is_empty()
        });

        let entries = self.seen.entry(fingerprint.to_string()).or_default();

        // Check for replay
        if entries.iter().any(|(n, _)| n == nonce) {
            return false;
        }

        if entries.len() >= MAX_NONCES_PER_FINGERPRINT {
            tracing::warn!(
                fingerprint = %fingerprint,
                "replay guard nonce ceiling reached; rejecting request"
            );
            return false;
        }

        entries.push((nonce.to_string(), now));
        true
    }
}

static REPLAY_GUARD: LazyLock<Mutex<ReplayGuard>> =
    LazyLock::new(|| Mutex::new(ReplayGuard::new()));

// ---------------------------------------------------------------------------
// Authorized key file loading
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct AuthorizedKey {
    public_key: VerifyingKey,
    scopes: Vec<String>,
    owner: String,
    principal_class: AuthorizedKeyPrincipalClass,
    configured_origin_site_id: Option<String>,
    source_file_hash: String,
    /// The verified grant, kept so its expiry can be judged once the caller's
    /// signature has proved it holds the key.
    grant: ryeos_node_runtime::identity::VerifiedAuthorizedKeyGrant,
}

/// Why `verify_request` refused a request.
#[derive(Debug)]
pub(crate) enum RequestAuthError {
    /// A plain refusal that tells the caller nothing beyond "unauthorized".
    Refused(String),
    /// The caller proved it holds the grant's key, and the grant is refused for
    /// a reason the holder is entitled to be told, with its cure.
    Grant(AuthorizedKeyError),
    /// The forwarding node proved it holds its key, and the delegation it
    /// presented is refused for a reason the node is entitled to be told, with
    /// its cure.
    Delegation(ryeos_node_runtime::remote_delegation::DelegationError),
}

impl std::fmt::Display for RequestAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => formatter.write_str(reason),
            Self::Grant(error) => write!(formatter, "{error}"),
            Self::Delegation(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<String> for RequestAuthError {
    fn from(reason: String) -> Self {
        Self::Refused(reason)
    }
}

impl From<&str> for RequestAuthError {
    fn from(reason: &str) -> Self {
        Self::Refused(reason.to_owned())
    }
}

/// Load a grant without judging its expiry. Request verification judges it
/// only after the signature check.
fn load_authorized_key_unjudged(
    fingerprint: &str,
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<AuthorizedKey> {
    let grant = ryeos_node_runtime::identity::load_verified_authorized_key_unjudged(
        fingerprint,
        auth_dir,
        node_identity,
    )?
    .ok_or_else(|| anyhow::anyhow!("unknown principal"))?;
    Ok(AuthorizedKey {
        public_key: grant.public_key,
        scopes: grant.scopes.clone(),
        owner: grant.owner.clone(),
        principal_class: grant.principal_class,
        configured_origin_site_id: grant.configured_origin_site_id.clone(),
        source_file_hash: grant.source_file_hash.clone(),
        grant,
    })
}

/// Load a grant that must not have lapsed.
fn load_authorized_key(
    fingerprint: &str,
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<AuthorizedKey> {
    let key = load_authorized_key_unjudged(fingerprint, auth_dir, node_identity)?;
    key.grant.ensure_unexpired(lillux::time::unix_secs_now())?;
    Ok(key)
}

/// Validate the complete authorized-key namespace during daemon startup.
/// Authentication is a boot invariant: accepting the node and discovering an
/// unusable stale grant only on the first request would leave an apparently
/// healthy daemon that rejects its operator. A grant that has merely lapsed is
/// not a boot failure: the invariant is integrity, not freshness.
pub fn validate_authorized_key_directory(
    auth_dir: &Path,
    node_identity: &NodeIdentity,
) -> Result<()> {
    let metadata = fs::symlink_metadata(auth_dir)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!(
            "authorized-key namespace is not a regular directory: {}",
            auth_dir.display()
        );
    }
    let mut entries = fs::read_dir(auth_dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            bail!(
                "authorized-key namespace contains a non-regular entry: {}",
                path.display()
            );
        }
        let fingerprint = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".toml"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "authorized-key namespace contains an unexpected entry: {}",
                    path.display()
                )
            })?;
        load_authorized_key_unjudged(fingerprint, auth_dir, node_identity).map_err(|error| {
            anyhow::anyhow!("invalid authorized-key grant {}: {error:#}", path.display())
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Request verification
// ---------------------------------------------------------------------------

fn canonical_path(uri: &axum::http::Uri) -> String {
    let path = uri.path();
    match uri.query() {
        None | Some("") => path.to_string(),
        Some(query) => {
            let mut params: Vec<(&str, &str)> = query
                .split('&')
                .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
                .collect();
            params.sort();
            let sorted: Vec<String> = params
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        k.to_string()
                    } else {
                        format!("{k}={v}")
                    }
                })
                .collect();
            format!("{path}?{}", sorted.join("&"))
        }
    }
}

/// Content hash of a `ryeos-request-v1` signed request: the bytes the caller
/// signs. It names one exact request (method, canonical path, body hash,
/// timestamp, nonce, audience), so it is also the request digest a remote
/// result attestation binds.
pub(crate) fn request_content_hash(
    method: &str,
    canonical_path: &str,
    body_hash: &str,
    timestamp: &str,
    nonce: &str,
    audience: &str,
) -> String {
    let string_to_sign = format!(
        "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        canonical_path,
        body_hash,
        timestamp,
        nonce,
        audience,
    );
    lillux::cas::sha256_hex(string_to_sign.as_bytes())
}

/// Recompute the request digest of a signed request this node has already
/// verified (`verify_request`). `None` when the signing headers are absent.
pub(crate) fn signed_request_digest(
    audience: &str,
    method: &str,
    uri: &axum::http::Uri,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Option<String> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
    };
    let timestamp = header("x-ryeos-timestamp")?;
    let nonce = header("x-ryeos-nonce")?;
    Some(request_content_hash(
        method,
        &canonical_path(uri),
        &lillux::cas::sha256_hex(body),
        timestamp,
        nonce,
        audience,
    ))
}

/// Hash the exact request facts authenticated by a source-node forwarding
/// co-signature. The primary signature is included so the source node attests
/// to this operator authorization, not merely to a coincident request body.
pub(crate) fn forwarding_request_content_hash(
    method: &str,
    canonical_path: &str,
    body_hash: &str,
    timestamp: &str,
    nonce: &str,
    audience: &str,
    primary_key_id: &str,
    primary_signature: &str,
    forwarding_key_id: &str,
    forwarding_site_id: &str,
) -> String {
    let string_to_sign = format!(
        "ryeos-forwarded-request-v1\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        canonical_path,
        body_hash,
        timestamp,
        nonce,
        audience,
        primary_key_id,
        primary_signature,
        forwarding_key_id,
        forwarding_site_id,
    );
    lillux::cas::sha256_hex(string_to_sign.as_bytes())
}

pub(crate) fn verify_request(
    state: &AppState,
    method: &str,
    uri: &axum::http::Uri,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<Principal, RequestAuthError> {
    let key_id = headers
        .get("x-ryeos-key-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let timestamp = headers
        .get("x-ryeos-timestamp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let nonce = headers
        .get("x-ryeos-nonce")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let signature = headers
        .get("x-ryeos-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let forwarding_key_id = headers
        .get("x-ryeos-forwarding-key-id")
        .and_then(|v| v.to_str().ok());
    let forwarding_site_id = headers
        .get("x-ryeos-forwarding-site-id")
        .and_then(|v| v.to_str().ok());
    let forwarding_signature = headers
        .get("x-ryeos-forwarding-signature")
        .and_then(|v| v.to_str().ok());
    let delegation_header = headers
        .get(ryeos_node_runtime::remote_delegation::DELEGATION_HEADER)
        .and_then(|v| v.to_str().ok());

    if key_id.is_empty() || timestamp.is_empty() || nonce.is_empty() || signature.is_empty() {
        return Err("missing auth headers".into());
    }

    // Extract fingerprint
    if !key_id.starts_with("fp:") {
        return Err("invalid key ID format".into());
    }
    let fingerprint = &key_id[3..];

    // Check timestamp freshness
    let req_time: u64 = timestamp
        .parse()
        .map_err(|_| "invalid timestamp".to_string())?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    if now.abs_diff(req_time) > TIMESTAMP_MAX_AGE_SECS {
        return Err("request expired".into());
    }

    // Load authorized key file. Expiry is judged only after the signature
    // check below, so a caller that does not hold the key learns nothing about
    // the grant.
    let auth_key = load_authorized_key_unjudged(
        fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .map_err(|e| format!("{e:#}"))?;
    if auth_key.principal_class == AuthorizedKeyPrincipalClass::RemoteNode {
        let configured_operator = NodeIdentity::load(&state.config.operator_signing_key_path)
            .map_err(|error| format!("load configured operator identity: {error:#}"))?;
        if configured_operator.fingerprint() == fingerprint {
            return Err(
                "configured operator key cannot authenticate through a remote_node grant".into(),
            );
        }
    }

    // Compute audience (this node's identity)
    let audience = state.identity.principal_id();

    // Build string-to-sign
    let body_hash = lillux::cas::sha256_hex(body);
    let canon = canonical_path(uri);
    let content_hash =
        request_content_hash(method, &canon, &body_hash, timestamp, nonce, &audience);

    // Verify signature
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .map_err(|_| "invalid signature encoding".to_string())?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| "invalid signature".to_string())?;
    auth_key
        .public_key
        .verify(content_hash.as_bytes(), &sig)
        .map_err(|_| "invalid signature".to_string())?;

    // The caller holds the key. A lapsed grant is refused with its cure, and a
    // grant without an end is honoured only for the configured operator.
    auth_key
        .grant
        .ensure_unexpired(lillux::time::unix_secs_now())
        .map_err(RequestAuthError::Grant)?;
    if auth_key.grant.expires_at == GrantExpiry::Never {
        let configured_operator = NodeIdentity::load(&state.config.operator_signing_key_path)
            .map_err(|error| format!("load configured operator identity: {error:#}"))?;
        if configured_operator.fingerprint() != fingerprint {
            return Err(RequestAuthError::Grant(
                AuthorizedKeyError::NeverExpiresNotOperator {
                    fingerprint: fingerprint.to_owned(),
                },
            ));
        }
    }

    let forwarding_headers_present = forwarding_key_id.is_some()
        || forwarding_site_id.is_some()
        || forwarding_signature.is_some();
    let (authenticated_site_id, forwarding_authority) = match auth_key.principal_class {
        AuthorizedKeyPrincipalClass::LocalClient => {
            if forwarding_headers_present || delegation_header.is_some() {
                return Err("local_client request cannot carry forwarding proof".into());
            }
            (None, None)
        }
        AuthorizedKeyPrincipalClass::RemoteNode if delegation_header.is_some() => {
            return verify_delegated(
                state,
                &DelegatedRequest {
                    method,
                    canonical_path: &canon,
                    body_hash: &body_hash,
                    timestamp,
                    nonce,
                    audience: &audience,
                    key_id,
                    signature,
                    forwarding_key_id,
                    forwarding_site_id,
                    forwarding_signature,
                    delegation: delegation_header.unwrap_or_default(),
                    request_digest: content_hash,
                },
                &auth_key,
            );
        }
        AuthorizedKeyPrincipalClass::RemoteNode => {
            if forwarding_headers_present {
                return Err("remote_node request cannot carry operator forwarding proof".into());
            }
            (auth_key.configured_origin_site_id.clone(), None)
        }
        AuthorizedKeyPrincipalClass::RemotePrincipal => {
            if delegation_header.is_some() {
                return Err(RequestAuthError::Delegation(DelegationError::NotTransitive));
            }
            let (forwarding_key_id, forwarding_site_id, forwarding_signature) =
                match (forwarding_key_id, forwarding_site_id, forwarding_signature) {
                    (Some(key_id), Some(site_id), Some(signature))
                        if !key_id.is_empty() && !site_id.is_empty() && !signature.is_empty() =>
                    {
                        (key_id, site_id, signature)
                    }
                    _ => {
                        return Err(
                        "remote_principal request requires a complete source-node forwarding proof"
                            .into(),
                    );
                    }
                };
            ryeos_node_runtime::identity::validate_canonical_site_id(forwarding_site_id)
                .map_err(|error| format!("{error:#}"))?;
            if auth_key.configured_origin_site_id.as_deref() != Some(forwarding_site_id) {
                return Err(
                    "source-node forwarding site does not match the remote_principal grant".into(),
                );
            }
            let forwarding_fingerprint = forwarding_key_id
                .strip_prefix("fp:")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "invalid forwarding key ID format".to_string())?;
            let forwarding_key = load_authorized_key(
                forwarding_fingerprint,
                &state.config.authorized_keys_dir,
                &state.identity,
            )
            .map_err(|error| format!("untrusted forwarding node: {error:#}"))?;
            if forwarding_key.principal_class != AuthorizedKeyPrincipalClass::RemoteNode {
                return Err("forwarding proof must be signed by an admitted remote_node".into());
            }
            if !forwarding_key.scopes.iter().any(|scope| {
                scope == ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE
            }) {
                return Err(
                    "forwarding-node grant lacks forwarded-operator attestation authority".into(),
                );
            }
            if forwarding_key.configured_origin_site_id.as_deref() != Some(forwarding_site_id) {
                return Err("forwarding-node grant does not match the asserted source site".into());
            }
            let forwarding_sig_bytes = base64::engine::general_purpose::STANDARD
                .decode(forwarding_signature)
                .map_err(|_| "invalid forwarding signature encoding".to_string())?;
            let forwarding_sig = Signature::from_slice(&forwarding_sig_bytes)
                .map_err(|_| "invalid forwarding signature".to_string())?;
            let forwarding_content_hash = forwarding_request_content_hash(
                method,
                &canon,
                &body_hash,
                timestamp,
                nonce,
                &audience,
                key_id,
                signature,
                forwarding_key_id,
                forwarding_site_id,
            );
            forwarding_key
                .public_key
                .verify(forwarding_content_hash.as_bytes(), &forwarding_sig)
                .map_err(|_| "invalid forwarding signature".to_string())?;
            (
                Some(forwarding_site_id.to_string()),
                Some(
                    ryeos_engine::principal_contract::ForwardingAuthorityEvidence {
                        source_node_fingerprint: forwarding_fingerprint.to_string(),
                        source_node_grant_hash: forwarding_key.source_file_hash,
                        delegation_digest: None,
                    },
                ),
            )
        }
    };

    // Replay check
    {
        let mut guard = match REPLAY_GUARD.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("replay guard mutex was poisoned, recovering");
                poisoned.into_inner()
            }
        };
        if !guard.check_and_record(fingerprint, nonce) {
            return Err("replayed request".into());
        }
    }

    Ok(Principal {
        request_digest: content_hash,
        fingerprint: fingerprint.to_string(),
        scopes: auth_key.scopes,
        owner: auth_key.owner,
        principal_class: auth_key.principal_class,
        authenticated_site_id,
        grant_authority: ryeos_engine::principal_contract::AuthenticatedGrantAuthority {
            principal_grant_hash: auth_key.source_file_hash,
            forwarding: forwarding_authority,
        },
        delegated_via: None,
    })
}

/// The facts of a request whose primary signer is a forwarding node acting
/// under a principal's delegation.
struct DelegatedRequest<'a> {
    method: &'a str,
    canonical_path: &'a str,
    body_hash: &'a str,
    timestamp: &'a str,
    nonce: &'a str,
    audience: &'a str,
    key_id: &'a str,
    signature: &'a str,
    forwarding_key_id: Option<&'a str>,
    forwarding_site_id: Option<&'a str>,
    forwarding_signature: Option<&'a str>,
    delegation: &'a str,
    request_digest: String,
}

/// Authenticate a request that a remote node forwarded for one of its
/// principals. The node's own signature has already verified. This admits the
/// principal from this node's grants, checks the node's co-signature and the
/// principal's signed delegation, and returns the principal with the authority
/// both its grant and its delegation allow.
fn verify_delegated(
    state: &AppState,
    request: &DelegatedRequest<'_>,
    node_key: &AuthorizedKey,
) -> Result<Principal, RequestAuthError> {
    use ryeos_node_runtime::remote_delegation as delegation_lib;
    let refuse = RequestAuthError::Delegation;

    let (Some(forwarding_key_id), Some(forwarding_site_id), Some(forwarding_signature)) = (
        request.forwarding_key_id,
        request.forwarding_site_id,
        request.forwarding_signature,
    ) else {
        return Err("delegated request requires a complete source-node forwarding proof".into());
    };
    if forwarding_key_id != request.key_id {
        return Err("a delegated request is signed and co-signed by the same node key".into());
    }
    ryeos_node_runtime::identity::validate_canonical_site_id(forwarding_site_id)
        .map_err(|error| format!("{error:#}"))?;
    if node_key.configured_origin_site_id.as_deref() != Some(forwarding_site_id) {
        return Err("forwarding site does not match the remote_node grant".into());
    }
    if !node_key
        .scopes
        .iter()
        .any(|scope| scope == ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE)
    {
        return Err("forwarding-node grant lacks forwarded-operator attestation authority".into());
    }
    let forwarding_sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(forwarding_signature)
        .map_err(|_| "invalid forwarding signature encoding".to_string())?;
    let forwarding_sig = Signature::from_slice(&forwarding_sig_bytes)
        .map_err(|_| "invalid forwarding signature".to_string())?;
    let forwarding_content_hash = forwarding_request_content_hash(
        request.method,
        request.canonical_path,
        request.body_hash,
        request.timestamp,
        request.nonce,
        request.audience,
        request.key_id,
        request.signature,
        forwarding_key_id,
        forwarding_site_id,
    );
    node_key
        .public_key
        .verify(forwarding_content_hash.as_bytes(), &forwarding_sig)
        .map_err(|_| "invalid forwarding signature".to_string())?;

    // From here the forwarding node has proved itself, so refusals name their
    // cure.
    let signed =
        delegation_lib::SignedDelegation::from_header_value(request.delegation).map_err(refuse)?;
    let own_site = state.threads.site_id();
    let own_node = state.identity.principal_id();
    signed
        .validate_at(
            lillux::time::unix_secs_now(),
            delegation_lib::DelegationParties {
                delegate_site: forwarding_site_id,
                delegate_node: request.key_id,
                audience_site: own_site,
                audience_node: &own_node,
            },
        )
        .map_err(refuse)?;

    let delegator = signed.body.delegator.clone();
    let delegator_fingerprint = signed.body.delegator_fingerprint().map_err(refuse)?;
    let principal_grant = load_authorized_key(
        delegator_fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .map_err(|error| match error.downcast::<AuthorizedKeyError>() {
        Ok(grant_error) => RequestAuthError::Grant(grant_error),
        Err(_) => refuse(DelegationError::PrincipalNotAdmitted {
            delegator: delegator.clone(),
        }),
    })?;
    if principal_grant.principal_class != AuthorizedKeyPrincipalClass::RemotePrincipal
        || principal_grant.configured_origin_site_id.as_deref() != Some(forwarding_site_id)
    {
        return Err(refuse(DelegationError::PrincipalNotAdmitted { delegator }));
    }
    signed.verify(&principal_grant.public_key).map_err(refuse)?;

    let digest = signed
        .digest()
        .map_err(|error| format!("delegation digest: {error:#}"))?;
    let revocations = ryeos_engine::roots::RuntimeRoot::new(state.config.app_root.clone())
        .delegation_revocations_dir();
    if delegation_lib::is_revoked(&revocations, &digest, &state.identity)
        .map_err(|error| format!("delegation revocation ledger: {error:#}"))?
    {
        return Err(refuse(DelegationError::Revoked { digest }));
    }

    let scopes = delegation_lib::intersect_scopes(&principal_grant.scopes, &signed.body.scopes);

    {
        let mut guard = match REPLAY_GUARD.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                tracing::warn!("replay guard mutex was poisoned, recovering");
                poisoned.into_inner()
            }
        };
        let node_fingerprint = request.key_id.strip_prefix("fp:").unwrap_or(request.key_id);
        if !guard.check_and_record(node_fingerprint, request.nonce) {
            return Err("replayed request".into());
        }
    }

    Ok(Principal {
        request_digest: request.request_digest.clone(),
        fingerprint: delegator_fingerprint.to_owned(),
        scopes,
        owner: principal_grant.owner,
        principal_class: AuthorizedKeyPrincipalClass::RemotePrincipal,
        authenticated_site_id: Some(forwarding_site_id.to_owned()),
        grant_authority: ryeos_engine::principal_contract::AuthenticatedGrantAuthority {
            principal_grant_hash: principal_grant.source_file_hash,
            forwarding: Some(
                ryeos_engine::principal_contract::ForwardingAuthorityEvidence {
                    source_node_fingerprint: request
                        .key_id
                        .strip_prefix("fp:")
                        .unwrap_or(request.key_id)
                        .to_owned(),
                    source_node_grant_hash: node_key.source_file_hash.clone(),
                    delegation_digest: Some(digest),
                },
            ),
        },
        delegated_via: Some(forwarding_site_id.to_owned()),
    })
}

// ---------------------------------------------------------------------------
// Auth middleware was deleted in v0.4.0.
// All auth now happens per-route inside the dispatcher's auth_invoker chain.
// verify_request() is still used by CompiledRyeosSignedVerifier.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use base64::Engine;
    use lillux::crypto::{EncodePrivateKey, SigningKey};
    use tempfile::TempDir;

    use super::*;
    use ryeos_node_runtime::identity::NodeIdentity;

    fn make_node_identity(sk: &SigningKey, dir: &std::path::Path) -> NodeIdentity {
        let pem = sk.to_pkcs8_pem(Default::default()).unwrap();
        let key_path = dir.join("node_key.pem");
        std::fs::write(&key_path, pem.as_bytes()).unwrap();
        NodeIdentity::load(&key_path).unwrap()
    }

    #[test]
    fn load_authorized_key_rejects_fingerprint_public_key_mismatch() {
        let real_subject = SigningKey::from_bytes(&[1u8; 32]);
        let attacker_subject = SigningKey::from_bytes(&[2u8; 32]);
        let node_signer = SigningKey::from_bytes(&[3u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let real_fp = lillux::signature::compute_fingerprint(&real_subject.verifying_key());
        let attacker_vk = attacker_subject.verifying_key();
        let attacker_key_b64 =
            base64::engine::general_purpose::STANDARD.encode(attacker_vk.as_bytes());

        let toml_body = format!(
            "schema_version = 3\nprincipal_class = \"local_client\"\nfingerprint = \"{real_fp}\"\npublic_key = \"ed25519:{attacker_key_b64}\"\nscopes = [\"*\"]\nlabel = \"evil\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\nexpires_at = \"2100-01-01T00:00:00Z\"\n[authorized_by]\nprincipal = \"fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n"
        );

        let signed = lillux::signature::sign_content_at(
            &toml_body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );

        let file_path = auth_dir.join(format!("{real_fp}.toml"));
        std::fs::write(&file_path, signed).unwrap();

        let result = load_authorized_key(&real_fp, &auth_dir, &node_identity);
        let err = result.expect_err("should reject mismatched fingerprint/public_key");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fingerprint"),
            "error message should mention 'fingerprint', got: {msg}"
        );
    }

    #[test]
    fn load_authorized_key_rejects_unversioned_grant() {
        let subject = SigningKey::from_bytes(&[4u8; 32]);
        let node_signer = SigningKey::from_bytes(&[5u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        let body = format!(
            "fingerprint = \"{fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"ryeos.execute.service.vault/list\"]\nlabel = \"old\"\n"
        );
        let signed = lillux::signature::sign_content_at(
            &body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );
        std::fs::write(auth_dir.join(format!("{fp}.toml")), signed).unwrap();

        let error = load_authorized_key(&fp, &auth_dir, &node_identity).unwrap_err();
        assert!(format!("{error:#}").contains("schema_version"));
    }

    /// Round-trip: write via canonical writer → load via auth loader → verify.
    #[test]
    fn canonical_writer_auth_loader_round_trip() {
        let client_key = SigningKey::from_bytes(&[42u8; 32]);
        let node_signer = SigningKey::from_bytes(&[99u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let client_vk = client_key.verifying_key();
        let client_fp = lillux::signature::compute_fingerprint(&client_vk);
        let client_key_b64 = base64::engine::general_purpose::STANDARD.encode(client_vk.as_bytes());

        // Write via canonical writer using REAL handler-required caps
        // (long-form `ryeos.execute.service.<subject>`). Short-form
        // scopes like "remote.admin" would load fine but never
        // authorize a real handler — see authorizer.rs.
        let scopes = vec![
            "ryeos.execute.service.remote/admin".to_string(),
            "ryeos.execute.service.bundle/install".to_string(),
        ];
        let _path = ryeos_node_runtime::identity::write_authorized_key_toml(
            &auth_dir,
            &client_fp,
            &client_key_b64,
            &scopes,
            "test-round-trip",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            &node_signer,
            ryeos_node_runtime::identity::WildcardPolicy::Reject,
        )
        .unwrap();

        // Load via auth loader
        let loaded = load_authorized_key(&client_fp, &auth_dir, &node_identity)
            .expect("auth loader should accept canonical writer output");

        // Verify contents
        assert_eq!(
            lillux::signature::compute_fingerprint(&loaded.public_key),
            client_fp,
            "loaded public key fingerprint must match"
        );
        assert_eq!(loaded.scopes, scopes, "loaded scopes must match");
        assert_eq!(loaded.owner, "test-round-trip");

        // Defense in depth: loaded scopes must actually satisfy a real
        // handler's required cap when fed to the authorizer.
        let authorizer = ryeos_runtime::authorizer::Authorizer::new();
        let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(
            "ryeos.execute.service.bundle/install",
        );
        authorizer
            .authorize(&loaded.scopes, &policy)
            .expect("real handler cap must be satisfied by loaded scopes");
    }

    #[test]
    fn remote_node_grant_round_trip_carries_authenticated_site() {
        let client_key = SigningKey::from_bytes(&[43u8; 32]);
        let node_signer = SigningKey::from_bytes(&[98u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        let client_vk = client_key.verifying_key();
        let client_fp = lillux::signature::compute_fingerprint(&client_vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(client_vk.as_bytes());

        ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
            &auth_dir,
            &client_fp,
            &key_b64,
            &["ryeos.execute.graph.arc/noop".to_string()],
            "remote",
            "admission:test",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            "site:origin",
            &node_signer,
        )
        .unwrap();

        let loaded = load_authorized_key(&client_fp, &auth_dir, &node_identity).unwrap();
        assert_eq!(
            loaded.principal_class,
            AuthorizedKeyPrincipalClass::RemoteNode
        );
        assert_eq!(
            loaded.configured_origin_site_id.as_deref(),
            Some("site:origin")
        );
    }

    #[test]
    fn remote_principal_grant_rejects_missing_or_noncanonical_origin() {
        let client_key = SigningKey::from_bytes(&[44u8; 32]);
        let node_signer = SigningKey::from_bytes(&[99u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let client_vk = client_key.verifying_key();
        let client_fp = lillux::signature::compute_fingerprint(&client_vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(client_vk.as_bytes());
        let path = auth_dir.join(format!("{client_fp}.toml"));

        for origin_line in ["", "origin_site_id = \"source\"\n"] {
            let body = format!(
                "schema_version = 3\nprincipal_class = \"remote_principal\"\n{origin_line}fingerprint = \"{client_fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"ryeos.execute.service.remote/run\"]\nlabel = \"remote operator\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\nexpires_at = \"2100-01-01T00:00:00Z\"\n[authorized_by]\nprincipal = \"fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n"
            );
            let signed = lillux::signature::sign_content_at(
                &body,
                &node_signer,
                "#",
                None,
                "2026-01-01T00:00:00Z",
            );
            std::fs::write(&path, signed).unwrap();
            assert!(load_authorized_key(&client_fp, &auth_dir, &node_identity).is_err());
        }
    }

    #[test]
    fn directory_validation_rejects_unexpected_entries() {
        let node_signer = SigningKey::from_bytes(&[97u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("README"), "not a grant").unwrap();

        let error = validate_authorized_key_directory(&auth_dir, &node_identity).unwrap_err();
        assert!(error.to_string().contains("unexpected entry"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_validation_rejects_symlinked_grants() {
        use std::os::unix::fs::symlink;

        let node_signer = SigningKey::from_bytes(&[96u8; 32]);
        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        let outside = tmp.path().join("outside.toml");
        std::fs::write(&outside, "not trusted").unwrap();
        symlink(&outside, auth_dir.join("fp:linked.toml")).unwrap();

        let error = validate_authorized_key_directory(&auth_dir, &node_identity).unwrap_err();
        assert!(error.to_string().contains("non-regular entry"));
    }

    /// Regression: a legitimately node-signed TOML that uses
    /// short-form scopes (`bundle.install` instead of
    /// `ryeos.execute.service.bundle/install`) must be REJECTED at
    /// load time, not silently loaded with useless scopes.
    ///
    /// Without this guard the request authenticates, then every
    /// handler 403s with a misleading "missing capability" message —
    /// the operator never sees that their TOML is broken.
    #[test]
    fn load_authorized_key_rejects_short_form_scopes() {
        let subject = SigningKey::from_bytes(&[7u8; 32]);
        let node_signer = SigningKey::from_bytes(&[8u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        // Hand-craft a TOML with short-form scopes that bypass the
        // canonical writer (which would reject them at write time).
        let body = format!(
            "schema_version = 3\nprincipal_class = \"local_client\"\nfingerprint = \"{fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"bundle.install\", \"remote.admin\"]\nlabel = \"old-short-form\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\nexpires_at = \"2100-01-01T00:00:00Z\"\n[authorized_by]\nprincipal = \"fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n"
        );
        let signed = lillux::signature::sign_content_at(
            &body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );
        let file_path = auth_dir.join(format!("{fp}.toml"));
        std::fs::write(&file_path, signed).unwrap();

        // Loader must REFUSE to load this file — not load with empty
        // or short-form scopes and silently fail every authorization.
        let err = load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect_err("short-form scope must be rejected at load");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("canonical") || msg.contains("ryeos."),
            "rejection message must mention canonical form, got: {msg}"
        );
        assert!(
            msg.contains("bundle.install") || msg.contains("not canonical"),
            "rejection message must point at the offending scope, got: {msg}"
        );
    }

    /// Canonical-form scopes load fine (positive control for the
    /// short-form regression above).
    #[test]
    fn load_authorized_key_accepts_canonical_scopes() {
        let subject = SigningKey::from_bytes(&[9u8; 32]);
        let node_signer = SigningKey::from_bytes(&[10u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        let body = format!(
            "schema_version = 3\nprincipal_class = \"local_client\"\nfingerprint = \"{fp}\"\npublic_key = \"ed25519:{key_b64}\"\nscopes = [\"ryeos.execute.service.vault/list\"]\nlabel = \"ok\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\nexpires_at = \"2100-01-01T00:00:00Z\"\n[authorized_by]\nprincipal = \"fp:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n"
        );
        let signed = lillux::signature::sign_content_at(
            &body,
            &node_signer,
            "#",
            None,
            "2026-01-01T00:00:00Z",
        );
        std::fs::write(auth_dir.join(format!("{fp}.toml")), signed).unwrap();

        let loaded = load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect("canonical scopes must load");
        assert_eq!(
            loaded.scopes,
            vec!["ryeos.execute.service.vault/list".to_string()]
        );
    }

    /// A legitimately node-signed key file whose BODY is modified after
    /// signing (here: an extra scope spliced into the array) must be
    /// rejected by the content-hash check — the original signature
    /// header no longer matches the body.
    #[test]
    fn authorized_key_rejects_tampered_body() {
        let subject = SigningKey::from_bytes(&[11u8; 32]);
        let node_signer = SigningKey::from_bytes(&[12u8; 32]);

        let tmp = TempDir::new().unwrap();
        let node_identity = make_node_identity(&node_signer, tmp.path());
        let auth_dir = tmp.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();

        let vk = subject.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());

        // Write a valid node-signed key file via the canonical writer.
        let file_path = ryeos_node_runtime::identity::write_authorized_key_toml(
            &auth_dir,
            &fp,
            &key_b64,
            &["ryeos.execute.service.vault/list".to_string()],
            "victim",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            &node_signer,
            ryeos_node_runtime::identity::WildcardPolicy::Reject,
        )
        .unwrap();

        // Sanity: the untampered file loads.
        load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect("untampered canonical file must load");

        // Tamper: keep the original signature header, but escalate the
        // scopes in the body.
        let raw = std::fs::read_to_string(&file_path).unwrap();
        let (sig_line, body) = raw.split_once('\n').unwrap();
        let tampered_body = body.replace(
            "\"ryeos.execute.service.vault/list\"",
            "\"ryeos.execute.service.vault/list\", \"ryeos.execute.service.bundle/install\"",
        );
        assert_ne!(body, tampered_body, "tamper must actually change the body");
        std::fs::write(&file_path, format!("{sig_line}\n{tampered_body}")).unwrap();

        let err = load_authorized_key(&fp, &auth_dir, &node_identity)
            .expect_err("body tampered after signing must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("tampered"),
            "error message should mention tampering, got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // verify_request: full request-verification path
    // ------------------------------------------------------------------

    /// Build a minimal AppState for verify_request tests. Mirrors the
    /// build_test_state helper in routes/invokers/none_invocation.rs.
    /// The Store owner sits between the directory and the application so
    /// destructuring drops the application before the Store closes.
    fn build_test_state() -> (
        TempDir,
        ryeos_app::test_support::TestStateStore,
        ryeos_app::state::AppState,
    ) {
        use std::sync::Arc;

        let tmpdir = TempDir::new().unwrap();
        let runtime_db_path =
            ryeos_app::test_support::TestStateStore::runtime_state_dir(tmpdir.path())
                .join("runtime.sqlite3");
        let key_path = tmpdir.path().join("identity").join("node-key.pem");
        let config = ryeos_node_runtime::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: runtime_db_path.clone(),
            uds_path: tmpdir.path().join("test.sock"),
            app_root: tmpdir.path().to_path_buf(),
            node_signing_key_path: key_path.clone(),
            operator_signing_key_path: tmpdir.path().join("user-key.pem"),
            authorized_keys_dir: tmpdir.path().join("auth"),
        };
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&key_path).unwrap();
        let signer = Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
            &identity,
        ));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
        let store_owner = ryeos_app::test_support::TestStateStore::open_with_barrier(
            tmpdir.path(),
            signer,
            write_barrier.clone(),
            Arc::new(head_trust),
        )
        .unwrap();
        let state_store = std::sync::Arc::clone(store_owner.store());
        let engine = Arc::new(ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            Vec::new(),
        ));
        let kind_profiles = Arc::new(ryeos_app::kind_profiles::KindProfileRegistry::build(None));
        let events = Arc::new(ryeos_app::event_store_service::EventStoreService::new(
            state_store.clone(),
        ));
        let event_streams = Arc::new(ryeos_app::event_stream::ThreadEventHub::new(16));
        let threads = Arc::new(
            ryeos_app::thread_lifecycle::ThreadLifecycleService::new_for_test_with_site_id(
                state_store.clone(),
                engine.clone(),
                kind_profiles.clone(),
                events.clone(),
                event_streams.clone(),
                "site:testhost",
            )
            .expect("valid test site identity"),
        );
        let commands = Arc::new(ryeos_app::command_service::CommandService::new(
            state_store.clone(),
            kind_profiles,
            events.clone(),
        ));
        let snapshot = ryeos_app::node_config::NodeConfigSnapshot {
            external_execution: Vec::new(),
            guest_runtime_materialization: Vec::new(),
            runtime_snapshot_production: Vec::new(),
            runtime_snapshot_qualification: Vec::new(),
            bundles: vec![],
            routes: vec![],
            commands: vec![],
        };
        let test_command_registry = Arc::new(
            ryeos_runtime::CommandRegistry::from_records(&[], &Default::default()).unwrap(),
        );
        let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());
        let controller_lifetime = store_owner.controller_lifetime();
        let state = ryeos_app::state::AppState {
            config: Arc::new(config),
            daemon_build: ryeos_app::build_info::get(),
            isolation: Arc::new(
                ryeos_engine::isolation::IsolationRuntime::disabled_for_authoring(),
            ),
            state_store,
            engine,
            resolution_cache: std::sync::Arc::new(
                ryeos_app::resolution_cache::ResolutionCache::new(128),
            ),
            engine_cache: ryeos_app::engine_cache::EngineCache::new(
                ryeos_app::engine_cache::EngineCacheConfig::default(),
            ),
            identity: Arc::new(identity),
            threads,
            live_input: Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new()),
            events,
            event_streams,
            commands,
            callback_tokens: Arc::new(ryeos_app::callback_token::CallbackCapabilityStore::new()),
            thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
            controller_lifetime,
            extensions: Arc::new(ryeos_app::extension_state::ExtensionState::new()),
            write_barrier: Arc::new(write_barrier),
            started_at: std::time::Instant::now(),
            started_at_iso: String::new(),
            catalog_health: ryeos_app::state::CatalogHealth {
                status: "ok".into(),
                missing_services: vec![],
            },
            services: Arc::new(crate::registry::build_service_registry()),
            service_descriptors: crate::handlers::ALL,
            node_config: Arc::new(snapshot.clone()),
            external_placement_backends: Arc::new(Default::default()),
            external_candidate_connectors: Arc::new(Default::default()),
            external_provider_configurations: Arc::new(Default::default()),
            external_candidate_imports: Arc::new(Default::default()),
            node_policy: Arc::new(
                ryeos_node_runtime::node_policy::NodePolicySnapshot::from_test_records(vec![
                    ryeos_node_runtime::node_policy::default_launch_trust_record(),
                    ryeos_node_runtime::node_policy::default_authorization_record(),
                    ryeos_node_runtime::node_policy::shipped_event_storage_record(),
                    Arc::new(
                        ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy(
                        ),
                    ),
                ]),
            ),
            vault: Arc::new(ryeos_app::vault::EmptyVault),
            provider_env: std::sync::Arc::new(ryeos_app::vault::ProviderEnvironment::none()),
            command_registry: test_command_registry,
            authorizer: test_auth,
            ignore_matcher: Arc::new(
                ryeos_app::ignore::IgnoreMatcher::from_config(&ryeos_app::ignore::IgnoreConfig {
                    patterns: Vec::new(),
                })
                .unwrap(),
            ),
            vault_fingerprint: None,
            accounting: None,
            persistent_sessions: Arc::new(
                ryeos_app::persistent_session::PersistentSessionPool::new(),
            ),
            execution_resources: Arc::new(
                ryeos_app::execution_resources::ExecutionResourcePool::deny_all(),
            ),
            scoped_producer_authorities: Arc::new(Default::default()),
            scoped_producer_processes: Arc::new(Default::default()),
            qualification_workspace_custody: Arc::new(Default::default()),
        };
        (tmpdir, store_owner, state)
    }

    /// Register `client_key` as an authorized principal via the
    /// canonical writer (signed by the node's own identity). Returns the
    /// client fingerprint.
    fn register_client_key(state: &ryeos_app::state::AppState, client_key: &SigningKey) -> String {
        let vk = client_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        ryeos_node_runtime::identity::write_authorized_key_toml(
            &state.config.authorized_keys_dir,
            &fp,
            &key_b64,
            &["ryeos.execute.service.vault/list".to_string()],
            "test-client",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            state.identity.signing_key(),
            ryeos_node_runtime::identity::WildcardPolicy::Reject,
        )
        .unwrap();
        fp
    }

    fn register_remote_principal_key(
        state: &ryeos_app::state::AppState,
        operator_key: &SigningKey,
        site_id: &str,
    ) -> String {
        register_remote_principal_key_with_scopes(
            state,
            operator_key,
            site_id,
            &["ryeos.execute.service.vault/list".to_string()],
        )
    }

    fn register_remote_principal_key_with_scopes(
        state: &ryeos_app::state::AppState,
        operator_key: &SigningKey,
        site_id: &str,
        scopes: &[String],
    ) -> String {
        let vk = operator_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes(
            &state.config.authorized_keys_dir,
            &fp,
            &key_b64,
            scopes,
            "test-remote-principal",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            &state.identity,
            ryeos_node_runtime::identity::WildcardPolicy::Reject,
            false,
            Some(site_id),
            false,
        )
        .unwrap();
        fp
    }

    fn register_remote_node_key(
        state: &ryeos_app::state::AppState,
        node_key: &SigningKey,
        site_id: &str,
    ) -> String {
        let vk = node_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
            &state.config.authorized_keys_dir,
            &fp,
            &key_b64,
            &[ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_string()],
            "test-forwarding-node",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            site_id,
            state.identity.signing_key(),
        )
        .unwrap();
        fp
    }

    /// Client-side signing mirroring remote/client.rs `sign_request`:
    /// Signature = Ed25519(sk, sha256(canonical_string).as_bytes()).
    /// The canonical string is computed over `signed_uri`, which may
    /// differ from the URI later presented to verify_request.
    fn signed_headers(
        client_key: &SigningKey,
        fingerprint: &str,
        method: &str,
        signed_uri: &axum::http::Uri,
        body: &[u8],
        timestamp: u64,
        nonce: &str,
        audience: &str,
    ) -> axum::http::HeaderMap {
        let body_hash = lillux::cas::sha256_hex(body);
        let canon = canonical_path(signed_uri);
        let string_to_sign = format!(
            "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
            method.to_uppercase(),
            canon,
            body_hash,
            timestamp,
            nonce,
            audience,
        );
        let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
        let sig = lillux::crypto::Signer::sign(client_key, content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-ryeos-key-id",
            format!("fp:{fingerprint}").parse().unwrap(),
        );
        headers.insert("x-ryeos-timestamp", timestamp.to_string().parse().unwrap());
        headers.insert("x-ryeos-nonce", nonce.parse().unwrap());
        headers.insert("x-ryeos-signature", sig_b64.parse().unwrap());
        headers
    }

    #[test]
    fn signed_request_digest_is_the_signed_content_hash() {
        let client_key = SigningKey::from_bytes(&[31u8; 32]);
        let uri: axum::http::Uri = "/execute?b=2&a=1".parse().unwrap();
        let body = br#"{"item_ref":"tool:example/run"}"#;
        let headers = signed_headers(
            &client_key,
            "client",
            "POST",
            &uri,
            body,
            1_700_000_000,
            "nonce-request-digest",
            "fp:audience",
        );
        let digest = signed_request_digest("fp:audience", "POST", &uri, &headers, body)
            .expect("signed headers present");
        let signature = base64::engine::general_purpose::STANDARD
            .decode(headers.get("x-ryeos-signature").unwrap().to_str().unwrap())
            .unwrap();
        client_key
            .verifying_key()
            .verify(
                digest.as_bytes(),
                &Signature::from_slice(&signature).unwrap(),
            )
            .expect("the request digest is exactly what the caller signed");
        assert_ne!(
            signed_request_digest("fp:audience", "POST", &uri, &headers, b"{}"),
            Some(digest),
            "a different body is a different request"
        );
        assert!(
            signed_request_digest(
                "fp:audience",
                "POST",
                &uri,
                &axum::http::HeaderMap::new(),
                body
            )
            .is_none()
        );
    }

    fn add_forwarding_proof(
        headers: &mut axum::http::HeaderMap,
        forwarding_key: &SigningKey,
        forwarding_fingerprint: &str,
        forwarding_site_id: &str,
        method: &str,
        uri: &axum::http::Uri,
        body: &[u8],
        audience: &str,
    ) {
        let header = |name: &str| headers.get(name).unwrap().to_str().unwrap().to_string();
        let primary_key_id = header("x-ryeos-key-id");
        let timestamp = header("x-ryeos-timestamp");
        let nonce = header("x-ryeos-nonce");
        let primary_signature = header("x-ryeos-signature");
        let forwarding_key_id = format!("fp:{forwarding_fingerprint}");
        let content_hash = forwarding_request_content_hash(
            method,
            &canonical_path(uri),
            &lillux::cas::sha256_hex(body),
            &timestamp,
            &nonce,
            audience,
            &primary_key_id,
            &primary_signature,
            &forwarding_key_id,
            forwarding_site_id,
        );
        let signature = lillux::crypto::Signer::sign(forwarding_key, content_hash.as_bytes());
        headers.insert(
            "x-ryeos-forwarding-key-id",
            forwarding_key_id.parse().unwrap(),
        );
        headers.insert(
            "x-ryeos-forwarding-site-id",
            forwarding_site_id.parse().unwrap(),
        );
        headers.insert(
            "x-ryeos-forwarding-signature",
            base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes())
                .parse()
                .unwrap(),
        );
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// A request whose timestamp is older than TIMESTAMP_MAX_AGE_SECS
    /// is rejected as expired; an otherwise-identical fresh request
    /// passes (positive control).
    #[test]
    fn replay_guard_rejects_stale_timestamp() {
        let (_tmp, _store_owner, state) = build_test_state();
        let client_key = SigningKey::from_bytes(&[21u8; 32]);
        let fp = register_client_key(&state, &client_key);
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/api/v1/status".parse().unwrap();

        // Stale: just past the freshness window.
        let stale_ts = unix_now() - (TIMESTAMP_MAX_AGE_SECS + 5);
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &uri,
            b"",
            stale_ts,
            "nonce-stale-timestamp-test",
            &audience,
        );
        let err = verify_request(&state, "GET", &uri, &headers, b"")
            .expect_err("stale timestamp must be rejected");
        assert!(
            err.to_string().contains("expired"),
            "stale-timestamp rejection should say expired, got: {err}"
        );

        // Fresh positive control: same key, same URI, current timestamp.
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &uri,
            b"",
            unix_now(),
            "nonce-fresh-timestamp-test",
            &audience,
        );
        let principal =
            verify_request(&state, "GET", &uri, &headers, b"").expect("fresh request must verify");
        assert_eq!(principal.fingerprint, fp);
    }

    /// Canonicalization sorts query params, so a signature computed
    /// over the sorted form verifies a request whose params arrive in
    /// a different textual order — while changing an actual param
    /// VALUE breaks the signature.
    #[test]
    fn signature_binds_query_param_order() {
        let (_tmp, _store_owner, state) = build_test_state();
        let client_key = SigningKey::from_bytes(&[22u8; 32]);
        let fp = register_client_key(&state, &client_key);
        let audience = state.identity.principal_id();

        let signed_uri: axum::http::Uri = "/api/v1/items?alpha=1&beta=2".parse().unwrap();
        let reordered_uri: axum::http::Uri = "/api/v1/items?beta=2&alpha=1".parse().unwrap();

        // Both spellings canonicalize identically.
        assert_eq!(canonical_path(&signed_uri), canonical_path(&reordered_uri));

        // Sign over the sorted form, present the reordered form: passes.
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &signed_uri,
            b"",
            unix_now(),
            "nonce-query-order-pass",
            &audience,
        );
        let principal = verify_request(&state, "GET", &reordered_uri, &headers, b"")
            .expect("reordered query params must still verify");
        assert_eq!(principal.fingerprint, fp);

        // Same signature, but an actually different param value: fails.
        let altered_uri: axum::http::Uri = "/api/v1/items?beta=3&alpha=1".parse().unwrap();
        let headers = signed_headers(
            &client_key,
            &fp,
            "GET",
            &signed_uri,
            b"",
            unix_now(),
            "nonce-query-order-fail",
            &audience,
        );
        let err = verify_request(&state, "GET", &altered_uri, &headers, b"")
            .expect_err("altered query param value must fail verification");
        assert!(
            err.to_string().contains("invalid signature"),
            "value change should break the signature, got: {err}"
        );
    }

    #[test]
    fn remote_principal_requires_and_verifies_source_node_cosignature() {
        let (_tmp, _store_owner, state) = build_test_state();
        let operator_key = SigningKey::from_bytes(&[23u8; 32]);
        let forwarding_key = SigningKey::from_bytes(&[24u8; 32]);
        let site_id = "site:source";
        let operator_fp = register_remote_principal_key(&state, &operator_key, site_id);
        let forwarding_fp = register_remote_node_key(&state, &forwarding_key, site_id);
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();
        let body = br#"{"item_ref":"test"}"#;

        let headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            body,
            unix_now(),
            "nonce-remote-principal-missing-proof",
            &audience,
        );
        let error = verify_request(&state, "POST", &uri, &headers, body)
            .expect_err("remote operator without a source-node proof must fail");
        assert!(error.to_string().contains("source-node forwarding proof"));

        let mut headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            body,
            unix_now(),
            "nonce-remote-principal-valid-proof",
            &audience,
        );
        add_forwarding_proof(
            &mut headers,
            &forwarding_key,
            &forwarding_fp,
            site_id,
            "POST",
            &uri,
            body,
            &audience,
        );
        let principal = verify_request(&state, "POST", &uri, &headers, body)
            .expect("co-signed remote operator request must verify");
        assert_eq!(
            principal.principal_class,
            AuthorizedKeyPrincipalClass::RemotePrincipal
        );
        assert_eq!(principal.authenticated_site_id.as_deref(), Some(site_id));
    }

    #[test]
    fn remote_principal_rejects_forwarding_proof_from_wrong_site() {
        let (_tmp, _store_owner, state) = build_test_state();
        let operator_key = SigningKey::from_bytes(&[25u8; 32]);
        let forwarding_key = SigningKey::from_bytes(&[26u8; 32]);
        let operator_fp =
            register_remote_principal_key(&state, &operator_key, "site:expected-source");
        let forwarding_fp =
            register_remote_node_key(&state, &forwarding_key, "site:different-source");
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();
        let body = br#"{"item_ref":"test"}"#;
        let mut headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            body,
            unix_now(),
            "nonce-remote-principal-wrong-site",
            &audience,
        );
        add_forwarding_proof(
            &mut headers,
            &forwarding_key,
            &forwarding_fp,
            "site:different-source",
            "POST",
            &uri,
            body,
            &audience,
        );
        let error = verify_request(&state, "POST", &uri, &headers, body)
            .expect_err("a different admitted site must not satisfy the operator grant");
        assert!(
            error
                .to_string()
                .contains("does not match the remote_principal grant")
        );
    }

    #[test]
    fn configured_operator_key_is_never_accepted_as_remote_node() {
        let (_tmp, _store_owner, state) = build_test_state();
        let operator_key = SigningKey::from_bytes(&[27u8; 32]);
        let pem = operator_key.to_pkcs8_pem(Default::default()).unwrap();
        std::fs::write(&state.config.operator_signing_key_path, pem.as_bytes()).unwrap();
        let operator_fp = register_remote_node_key(&state, &operator_key, "site:source");
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();
        let headers = signed_headers(
            &operator_key,
            &operator_fp,
            "POST",
            &uri,
            b"{}",
            unix_now(),
            "nonce-configured-operator-remote-node",
            &audience,
        );
        let error = verify_request(&state, "POST", &uri, &headers, b"{}")
            .expect_err("configured operator remote_node confusion must fail");
        assert!(
            error
                .to_string()
                .contains("cannot authenticate through a remote_node grant")
        );
    }

    /// Write a grant for `client_key` that lapsed long ago.
    fn register_expired_client_key(
        state: &ryeos_app::state::AppState,
        client_key: &SigningKey,
    ) -> String {
        let vk = client_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(vk.as_bytes());
        ryeos_node_runtime::identity::create_authorized_key_toml(
            &state.config.authorized_keys_dir,
            &fp,
            &key_b64,
            &["ryeos.execute.service.vault/list".to_string()],
            "lapsed-client",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms {
                expires_at: GrantExpiry::At(1_000),
                authorized_by: ryeos_node_runtime::identity::AuthorizedBy::principal_request(
                    &"a".repeat(64),
                    None,
                ),
            },
            state.identity.signing_key(),
        )
        .unwrap();
        fp
    }

    #[test]
    fn expired_grant_reveals_expiry_only_after_signature_check() {
        let (_tmp, _store_owner, state) = build_test_state();
        let client_key = SigningKey::from_bytes(&[81u8; 32]);
        let impostor_key = SigningKey::from_bytes(&[82u8; 32]);
        let fp = register_expired_client_key(&state, &client_key);
        let audience = state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();

        // The key holder is told its grant lapsed.
        let headers = signed_headers(
            &client_key,
            &fp,
            "POST",
            &uri,
            b"{}",
            unix_now(),
            "nonce-expired-holder",
            &audience,
        );
        match verify_request(&state, "POST", &uri, &headers, b"{}") {
            Err(RequestAuthError::Grant(error)) => {
                assert_eq!(error.code(), "grant_expired");
                assert!(error.to_string().contains("renew"), "{error}");
            }
            other => panic!("expected a typed expiry refusal, got {other:?}"),
        }

        // Someone who names the fingerprint but cannot sign for it is told
        // nothing about the grant.
        let headers = signed_headers(
            &impostor_key,
            &fp,
            "POST",
            &uri,
            b"{}",
            unix_now(),
            "nonce-expired-impostor",
            &audience,
        );
        match verify_request(&state, "POST", &uri, &headers, b"{}") {
            Err(RequestAuthError::Refused(reason)) => {
                assert!(reason.contains("invalid signature"), "{reason}");
            }
            other => panic!("expected a plain refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_lapsed_grant_is_not_a_boot_failure_but_a_stale_one_is() {
        let (_tmp, _store_owner, state) = build_test_state();
        let client_key = SigningKey::from_bytes(&[83u8; 32]);
        register_expired_client_key(&state, &client_key);
        validate_authorized_key_directory(&state.config.authorized_keys_dir, &state.identity)
            .expect("a lapsed grant keeps its integrity");

        let stale_key = SigningKey::from_bytes(&[84u8; 32]);
        let vk = stale_key.verifying_key();
        let fp = lillux::signature::compute_fingerprint(&vk);
        let body = format!(
            "schema_version = 2\nprincipal_class = \"local_client\"\nfingerprint = \"{fp}\"\npublic_key = \"ed25519:{}\"\nscopes = [\"ryeos.execute.service.vault/list\"]\nlabel = \"old\"\ngranted_by = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n",
            base64::engine::general_purpose::STANDARD.encode(vk.as_bytes())
        );
        std::fs::write(
            state.config.authorized_keys_dir.join(format!("{fp}.toml")),
            lillux::signature::sign_content(&body, state.identity.signing_key(), "#", None),
        )
        .unwrap();
        let error =
            validate_authorized_key_directory(&state.config.authorized_keys_dir, &state.identity)
                .expect_err("a stale grant keeps the daemon from starting");
        assert!(format!("{error:#}").contains("re-issue"), "{error:#}");
    }

    // ------------------------------------------------------------------
    // Delegated requests
    // ------------------------------------------------------------------

    const SITE_A: &str = "site:source";
    const GRANT_SCOPES: [&str; 3] = [
        "ryeos.execute.service.remote/run",
        "ryeos.execute.service.remote/push",
        "ryeos.execute.service.vault/list",
    ];

    /// A node B holding one forwarding node admitted as `remote_node` for
    /// site A, and one principal admitted as `remote_principal` for site A.
    struct Delegated {
        // The application drops before the store that owns its state, which
        // drops before the directory that holds both.
        state: ryeos_app::state::AppState,
        _store_owner: ryeos_app::test_support::TestStateStore,
        _tmp: TempDir,
        user_key: SigningKey,
        node_key: SigningKey,
        node_fp: String,
        user_fp: String,
    }

    impl Delegated {
        fn new(seed: u8) -> Self {
            let (tmp, store_owner, state) = build_test_state();
            // Request verification reads the configured operator's identity to
            // refuse its key in a `remote_node` grant.
            NodeIdentity::create(&state.config.operator_signing_key_path).unwrap();
            let user_key = SigningKey::from_bytes(&[seed; 32]);
            let node_key = SigningKey::from_bytes(&[seed + 1; 32]);
            let scopes: Vec<String> = GRANT_SCOPES.iter().map(|s| (*s).to_owned()).collect();
            let user_fp =
                register_remote_principal_key_with_scopes(&state, &user_key, SITE_A, &scopes);
            let node_fp = register_remote_node_key(&state, &node_key, SITE_A);
            Self {
                state,
                _store_owner: store_owner,
                _tmp: tmp,
                user_key,
                node_key,
                node_fp,
                user_fp,
            }
        }

        fn delegation(
            &self,
            scopes: &[&str],
            not_after: u64,
        ) -> ryeos_node_runtime::remote_delegation::SignedDelegation {
            self.delegation_to(scopes, not_after, self.state.threads.site_id())
        }

        fn delegation_to(
            &self,
            scopes: &[&str],
            not_after: u64,
            audience_site: &str,
        ) -> ryeos_node_runtime::remote_delegation::SignedDelegation {
            use ryeos_node_runtime::remote_delegation::{
                DelegationBody, DelegationParties, SignedDelegation,
            };
            let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
            let body = DelegationBody::new(
                &format!("fp:{}", self.user_fp),
                DelegationParties {
                    delegate_site: SITE_A,
                    delegate_node: &format!("fp:{}", self.node_fp),
                    audience_site,
                    audience_node: &self.state.identity.principal_id(),
                },
                &scopes,
                not_after,
            );
            SignedDelegation::sign(body, &self.user_key).unwrap()
        }

        /// A request the forwarding node signs and co-signs, carrying the
        /// delegation.
        fn request(
            &self,
            nonce: &str,
            delegation: Option<&ryeos_node_runtime::remote_delegation::SignedDelegation>,
            cosign: bool,
        ) -> (axum::http::Uri, axum::http::HeaderMap, &'static [u8]) {
            let audience = self.state.identity.principal_id();
            let uri: axum::http::Uri = "/execute".parse().unwrap();
            let body: &'static [u8] = br#"{"item_ref":"test"}"#;
            let mut headers = signed_headers(
                &self.node_key,
                &self.node_fp,
                "POST",
                &uri,
                body,
                unix_now(),
                nonce,
                &audience,
            );
            if cosign {
                add_forwarding_proof(
                    &mut headers,
                    &self.node_key,
                    &self.node_fp,
                    SITE_A,
                    "POST",
                    &uri,
                    body,
                    &audience,
                );
            }
            if let Some(delegation) = delegation {
                headers.insert(
                    ryeos_node_runtime::remote_delegation::DELEGATION_HEADER,
                    delegation.to_header_value().unwrap().parse().unwrap(),
                );
            }
            (uri, headers, body)
        }

        fn verify(
            &self,
            nonce: &str,
            delegation: Option<&ryeos_node_runtime::remote_delegation::SignedDelegation>,
            cosign: bool,
        ) -> Result<Principal, RequestAuthError> {
            let (uri, headers, body) = self.request(nonce, delegation, cosign);
            verify_request(&self.state, "POST", &uri, &headers, body)
        }
    }

    fn refusal_code(error: &RequestAuthError) -> Option<&'static str> {
        match error {
            RequestAuthError::Delegation(error) => Some(error.code()),
            _ => None,
        }
    }

    #[test]
    fn delegated_request_requires_forwarding_cosignature() {
        let node = Delegated::new(40);
        let delegation = node.delegation(&["ryeos.execute.service.remote/run"], unix_now() + 600);
        let error = node
            .verify("nonce-delegated-no-cosign", Some(&delegation), false)
            .expect_err("a delegated request without the co-signature must fail");
        assert!(error.to_string().contains("forwarding proof"), "{error}");
        node.verify("nonce-delegated-cosign", Some(&delegation), true)
            .expect("the same request with the co-signature verifies");
    }

    #[test]
    fn delegated_request_principal_is_delegator() {
        let node = Delegated::new(42);
        let delegation = node.delegation(&["ryeos.execute.service.remote/run"], unix_now() + 600);
        let principal = node
            .verify("nonce-delegated-principal", Some(&delegation), true)
            .unwrap();
        assert_eq!(principal.fingerprint, node.user_fp);
        assert_eq!(
            principal.principal_class,
            AuthorizedKeyPrincipalClass::RemotePrincipal
        );
        assert_eq!(principal.authenticated_site_id.as_deref(), Some(SITE_A));
        assert_eq!(principal.delegated_via.as_deref(), Some(SITE_A));
        let forwarding = principal.grant_authority.forwarding.unwrap();
        assert_eq!(forwarding.source_node_fingerprint, node.node_fp);
        assert_eq!(
            forwarding.delegation_digest,
            Some(delegation.digest().unwrap())
        );
    }

    #[test]
    fn delegated_request_scopes_are_grant_and_delegation_intersection() {
        let node = Delegated::new(44);
        let delegation = node.delegation(
            &[
                "ryeos.execute.service.remote/run",
                "ryeos.execute.service.remote/pull",
                "ryeos.execute.tool.*",
            ],
            unix_now() + 600,
        );
        let principal = node
            .verify("nonce-delegated-scopes", Some(&delegation), true)
            .unwrap();
        assert_eq!(
            principal.scopes,
            vec!["ryeos.execute.service.remote/run".to_owned()],
            "the delegation lends tool scopes the grant never held, and withholds the rest of the grant"
        );
    }

    #[test]
    fn delegated_forwarded_request_is_not_transitive() {
        let node = Delegated::new(46);
        let delegation = node.delegation(&["ryeos.execute.service.remote/run"], unix_now() + 600);
        // The primary signer is the principal's own key (a request that was
        // already forwarded once), and it also carries a delegation.
        let audience = node.state.identity.principal_id();
        let uri: axum::http::Uri = "/execute".parse().unwrap();
        let body = br#"{"item_ref":"test"}"#;
        let mut headers = signed_headers(
            &node.user_key,
            &node.user_fp,
            "POST",
            &uri,
            body,
            unix_now(),
            "nonce-delegated-transitive",
            &audience,
        );
        add_forwarding_proof(
            &mut headers,
            &node.node_key,
            &node.node_fp,
            SITE_A,
            "POST",
            &uri,
            body,
            &audience,
        );
        headers.insert(
            ryeos_node_runtime::remote_delegation::DELEGATION_HEADER,
            delegation.to_header_value().unwrap().parse().unwrap(),
        );
        let error = verify_request(&node.state, "POST", &uri, &headers, body).unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some("remote_delegation_not_transitive")
        );
    }

    #[test]
    fn delegated_request_refuses_wrong_audience_expiry_and_tamper() {
        let node = Delegated::new(48);
        let wrong_audience = node.delegation_to(
            &["ryeos.execute.service.remote/run"],
            unix_now() + 600,
            "site:elsewhere",
        );
        let error = node
            .verify("nonce-delegated-audience", Some(&wrong_audience), true)
            .unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some("remote_delegation_wrong_audience")
        );

        let expired = node.delegation(&["ryeos.execute.service.remote/run"], 1_000);
        let error = node
            .verify("nonce-delegated-expired", Some(&expired), true)
            .unwrap_err();
        assert_eq!(refusal_code(&error), Some("remote_delegation_expired"));

        let mut tampered = node.delegation(&["ryeos.execute.service.remote/run"], unix_now() + 600);
        tampered.body.scopes = vec!["ryeos.execute.service.remote/push".to_owned()];
        let error = node
            .verify("nonce-delegated-tamper", Some(&tampered), true)
            .unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some("remote_delegation_bad_signature")
        );
    }

    #[test]
    fn remote_principal_grant_must_name_origin_site() {
        let node = Delegated::new(50);
        // The delegator is admitted for a different origin site than the
        // forwarding node's.
        let other_user = SigningKey::from_bytes(&[90u8; 32]);
        let other_fp = register_remote_principal_key_with_scopes(
            &node.state,
            &other_user,
            "site:another-source",
            &["ryeos.execute.service.remote/run".to_owned()],
        );
        use ryeos_node_runtime::remote_delegation::{
            DelegationBody, DelegationParties, SignedDelegation,
        };
        let body = DelegationBody::new(
            &format!("fp:{other_fp}"),
            DelegationParties {
                delegate_site: SITE_A,
                delegate_node: &format!("fp:{}", node.node_fp),
                audience_site: node.state.threads.site_id(),
                audience_node: &node.state.identity.principal_id(),
            },
            &["ryeos.execute.service.remote/run".to_owned()],
            unix_now() + 600,
        );
        let delegation = SignedDelegation::sign(body, &other_user).unwrap();
        let error = node
            .verify("nonce-delegated-origin", Some(&delegation), true)
            .unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some("remote_delegation_principal_not_admitted")
        );
        // A delegator with no grant at all is refused the same way.
        let stranger = SigningKey::from_bytes(&[91u8; 32]);
        let body = DelegationBody::new(
            &format!(
                "fp:{}",
                lillux::signature::compute_fingerprint(&stranger.verifying_key())
            ),
            DelegationParties {
                delegate_site: SITE_A,
                delegate_node: &format!("fp:{}", node.node_fp),
                audience_site: node.state.threads.site_id(),
                audience_node: &node.state.identity.principal_id(),
            },
            &["ryeos.execute.service.remote/run".to_owned()],
            unix_now() + 600,
        );
        let delegation = SignedDelegation::sign(body, &stranger).unwrap();
        let error = node
            .verify("nonce-delegated-stranger", Some(&delegation), true)
            .unwrap_err();
        assert_eq!(
            refusal_code(&error),
            Some("remote_delegation_principal_not_admitted")
        );
    }

    #[test]
    fn a_tombstoned_delegation_is_refused_and_others_are_not() {
        let node = Delegated::new(52);
        let revoked = node.delegation(&["ryeos.execute.service.remote/run"], unix_now() + 600);
        let kept = node.delegation(&["ryeos.execute.service.remote/run"], unix_now() + 600);
        node.verify("nonce-delegated-before-revoke", Some(&revoked), true)
            .unwrap();
        let revocations = ryeos_engine::roots::RuntimeRoot::new(node.state.config.app_root.clone())
            .delegation_revocations_dir();
        std::fs::create_dir_all(revocations.parent().unwrap()).unwrap();
        ryeos_node_runtime::remote_delegation::write_tombstone(
            &revocations,
            &revoked.digest().unwrap(),
            &revoked.body.delegator,
            &revoked.body.delegator,
            None,
            None,
            &node.state.identity,
            unix_now(),
        )
        .unwrap();
        let error = node
            .verify("nonce-delegated-after-revoke", Some(&revoked), true)
            .unwrap_err();
        assert_eq!(refusal_code(&error), Some("remote_delegation_revoked"));
        node.verify("nonce-delegated-other", Some(&kept), true)
            .expect("only the tombstoned digest is refused");
    }

    #[test]
    fn delegated_verification_does_not_depend_on_how_the_origin_is_pinned() {
        // The receiving node admits the forwarding node from its grants and the
        // delegation alone. An origin this node pinned inbound-only (a node
        // that only calls in) must still verify.
        let node = Delegated::new(54);
        let origin = SigningKey::from_bytes(&[92u8; 32]);
        let public =
            base64::engine::general_purpose::STANDARD.encode(origin.verifying_key().as_bytes());
        crate::remote::config::write_remote_pin(
            &node.state.config.app_root,
            node.state.identity.signing_key(),
            &crate::remote::config::RemoteConfig {
                name: "origin".to_owned(),
                url: None,
                reachability: crate::remote::config::Reachability::InboundOnly,
                principal_id: format!(
                    "fp:{}",
                    lillux::signature::compute_fingerprint(&origin.verifying_key())
                ),
                signing_key: format!("ed25519:{public}"),
                site_id: SITE_A.to_owned(),
                vault_fingerprint: "vault".to_owned(),
                ingest_ignore: None,
                project_bindings: Default::default(),
            },
            crate::remote::config::PinSource::DescriptorWithoutContact,
            None,
        )
        .unwrap();
        let delegation = node.delegation(&["ryeos.execute.service.remote/run"], unix_now() + 600);
        let principal = node
            .verify("nonce-delegated-inbound-only", Some(&delegation), true)
            .expect("an inbound-only pin of the origin does not stop inbound verification");
        assert_eq!(principal.fingerprint, node.user_fp);
    }
}
