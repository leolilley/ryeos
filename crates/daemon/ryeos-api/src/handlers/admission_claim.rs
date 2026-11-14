//! `admission/claim` — claim a one-time node-local admission token.
//!
//! This is deliberately small: it does not introduce a central auth root
//! or a new permission system. A provider/operator can place one-time
//! token files on the target node; the claimant proves possession of the
//! private key for the public key being admitted; the node then writes the
//! normal node-signed authorized-key grant.
//!
//! A token names the class of grant it admits. A `remote_principal` token
//! also names the origin site, and its claim must carry a co-signature from
//! an admitted node of that site over the claimant's signed claim, the same
//! attestation a forwarded request carries.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use lillux::crypto::{Signature, Verifier, VerifyingKey};
use ryeos_node_runtime::identity::{AuthorizedKeyPrincipalClass, AuthorizedKeySubject};
use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const CLAIM_MAX_AGE_SECS: u64 = 300;
const ADMISSION_CLAIM_PROTOCOL_VERSION: u32 = 3;
/// Admission token format this node reads. Any other version is refused as
/// stale, because the class of a token's grant is part of the stored format.
const ADMISSION_TOKEN_FORMAT_VERSION: u32 =
    ryeos_core_tools::actions::authorize::ADMISSION_TOKEN_FORMAT_VERSION;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Exact admission claim protocol. Version 3 binds the grant class and
    /// the claimant's site identity into the self-signature.
    pub protocol_version: u32,
    /// Opaque one-time admission token delivered out-of-band by the
    /// target node operator or provider/provisioner.
    pub token: String,
    /// Ed25519 public key in `ed25519:<base64>` format. This is the key
    /// that future signed requests from the admitted key will use.
    pub public_key: String,
    /// Class of grant the claimant asks for. Must equal the class the token
    /// admits; it is covered by the claim signature.
    pub principal_class: AuthorizedKeyPrincipalClass,
    /// Site identity asserted by the claimant and covered by its claim
    /// signature. The destination re-signs this binding into the grant.
    /// Required for remote classes, and absent for `local_client`.
    #[serde(default)]
    pub origin_site_id: Option<String>,
    /// `fp:<hex>` of the origin node that co-signed a `remote_principal`
    /// claim. Absent for other classes.
    #[serde(default)]
    pub forwarding_key_id: Option<String>,
    /// Base64 co-signature by that node, made over the hash built by
    /// `cosignature_content_hash`. Absent for other classes.
    #[serde(default)]
    pub forwarding_signature: Option<String>,
    /// Human-readable label for the resulting authorized-key grant.
    #[serde(default)]
    pub label: Option<String>,
    /// Capabilities requested by the claimant. Must be a subset of the
    /// scopes allowed by the local token file.
    #[serde(deserialize_with = "ryeos_runtime::scalar_or_vec::deserialize")]
    pub scopes: Vec<String>,
    /// Unix timestamp included in the claimant signature.
    pub signed_at: u64,
    /// Claimant nonce included in the claimant signature.
    pub nonce: String,
    /// Base64 Ed25519 signature by `public_key` over the admission claim
    /// string produced by `claim_string`.
    pub signature: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionTokenFile {
    #[serde(rename = "version")]
    _version: u32,
    token_hash: String,
    #[serde(default)]
    label: Option<String>,
    scopes: Vec<String>,
    #[serde(default)]
    issued_at_unix: Option<u64>,
    #[serde(default)]
    ttl_secs: Option<u64>,
    expires_at_unix: u64,
    /// Class of the grant redeeming this token writes.
    class: AuthorizedKeyPrincipalClass,
    /// Site a `remote_principal` token is bound to.
    #[serde(default)]
    origin_site_id: Option<String>,
    /// Lifetime requested for the admitted grant; the node's authorization
    /// policy bounds it when the token is redeemed.
    #[serde(default)]
    grant_lifetime_secs: Option<u64>,
}

/// Just the version of a stored token, whatever else it holds, so a token of
/// a retired format is named stale instead of failing to parse.
#[derive(serde::Deserialize)]
struct AdmissionTokenVersion {
    version: u32,
}

/// A typed refusal that names its cure.
fn refusal(code: &str, status: u16, message: String) -> HandlerError {
    HandlerError::Structured {
        code: code.to_owned(),
        status,
        body: serde_json::json!({
            "error": message,
            "error_code": code,
        }),
    }
}

#[derive(serde::Serialize)]
pub struct Response {
    pub admitted: bool,
    pub fingerprint: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub granted_by: String,
    pub created_at: String,
    pub principal_class: AuthorizedKeyPrincipalClass,
    pub origin_site_id: Option<String>,
    /// RFC 3339 UTC time at which the admitted grant stops authenticating.
    pub expires_at: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    if req.protocol_version != ADMISSION_CLAIM_PROTOCOL_VERSION {
        return Err(HandlerError::BadRequest(format!(
            "admission claim protocol_version must be exactly {} (got {})",
            ADMISSION_CLAIM_PROTOCOL_VERSION, req.protocol_version
        )));
    }
    if let Some(origin_site_id) = req.origin_site_id.as_deref()
        && ryeos_node_runtime::identity::validate_canonical_site_id(origin_site_id).is_err()
    {
        return Err(HandlerError::BadRequest(
            "origin_site_id must use the canonical `site:<name>` form".to_string(),
        ));
    }
    let maximum_token_ttl_secs = hosted_admission_token_ttl_secs(&state)?;
    let token_hash = token_hash(&req.token);
    let token_path = admission_token_path(&state.config.app_root, &token_hash);
    let token = read_token_file(&token_path)?;

    if token.token_hash != token_hash {
        return Err(HandlerError::BadRequest(
            "admission token hash mismatch".to_string(),
        ));
    }

    let now = now_unix();
    if token.expires_at_unix < now {
        return Err(HandlerError::Forbidden(
            "admission token expired".to_string(),
        ));
    }
    enforce_hosted_policy_token_ttl(maximum_token_ttl_secs, &token, now)?;
    let origin_site_id = check_claim_matches_token(&req, &token)?;
    if now.abs_diff(req.signed_at) > CLAIM_MAX_AGE_SECS {
        return Err(HandlerError::Forbidden(
            "admission claim signature timestamp expired".to_string(),
        ));
    }
    if req.nonce.trim().is_empty() {
        return Err(HandlerError::BadRequest("nonce must not be empty".into()));
    }

    let (key_b64, verifying_key, fingerprint) = parse_public_key(&req.public_key)?;
    let configured_operator = ryeos_node_runtime::identity::NodeIdentity::load(
        &state.config.operator_signing_key_path,
    )
    .map_err(|error| HandlerError::Internal(format!("load configured operator: {error:#}")))?;
    if fingerprint == configured_operator.fingerprint() {
        return Err(HandlerError::Forbidden(
            "the configured operator cannot be admitted by token; its grant is managed by init and the stopped-daemon offline conversion path"
                .to_string(),
        ));
    }
    let scopes = normalize_scopes(&req.scopes, "admission claim requests")?;
    let allowed_scopes = normalize_scopes(&token.scopes, "admission token files")?;
    ensure_scope_subset(&scopes, &allowed_scopes, &state)?;
    let claim_digest = verify_claim_signature(&req, &token_hash, &scopes, &verifying_key, &state)?;
    if token.class == AuthorizedKeyPrincipalClass::RemotePrincipal {
        verify_origin_cosignature(
            &req,
            &claim_digest,
            &fingerprint,
            origin_site_id.unwrap_or_default(),
            &state,
        )?;
    }

    let label = req
        .label
        .clone()
        .or(token.label.clone())
        .unwrap_or_else(|| {
            format!(
                "admitted-node-{}",
                fingerprint.chars().take(12).collect::<String>()
            )
        });
    if label.trim().is_empty() {
        return Err(HandlerError::BadRequest("label must not be empty".into()));
    }

    // The admitted key asked for this grant by signing its claim; the token it
    // redeemed is the authority that lets the claim through. The grant takes
    // the token's class, and lives for the token's requested lifetime or the
    // class maximum, whichever the node's authorization policy allows.
    let terms = super::grant_terms::issuance_terms(
        &state,
        token.class,
        token.grant_lifetime_secs,
        ryeos_node_runtime::identity::AuthorizedBy::principal_request(
            &fingerprint,
            Some(claim_digest),
        ),
    )?;

    consume_token_file(&token_path)?;

    let created_at = lillux::time::iso8601_now();
    let auth_dir = state.config.authorized_keys_dir.clone();
    let subject = match (token.class, origin_site_id) {
        (AuthorizedKeyPrincipalClass::LocalClient, _) => AuthorizedKeySubject::LocalClient,
        (AuthorizedKeyPrincipalClass::RemoteNode, Some(origin_site_id)) => {
            AuthorizedKeySubject::RemoteNode { origin_site_id }
        }
        (AuthorizedKeyPrincipalClass::RemotePrincipal, Some(origin_site_id)) => {
            AuthorizedKeySubject::RemotePrincipal { origin_site_id }
        }
        (_, None) => {
            return Err(HandlerError::Internal(
                "remote admission reached the grant write without an origin site".to_string(),
            ));
        }
    };
    let write_result = ryeos_node_runtime::identity::create_authorized_key_toml_for_subject(
        &auth_dir,
        &fingerprint,
        &key_b64,
        &scopes,
        &label,
        &format!(
            "admission:{}",
            token_hash.chars().take(12).collect::<String>()
        ),
        &created_at,
        &terms,
        state.identity.signing_key(),
        subject,
    );
    // The token rename above is the single-winner point and must stay first.
    // If the grant was not written, give the token back so the claim can be
    // retried instead of burning the one-time token.
    if write_result.is_err() {
        restore_token_file(&token_path);
    }
    write_result
        .map_err(|e| {
        if matches!(
            e.downcast_ref::<ryeos_node_runtime::identity::AuthorizedKeyCreateError>(),
            Some(ryeos_node_runtime::identity::AuthorizedKeyCreateError::AlreadyExists)
        ) {
            HandlerError::Conflict(
                "authorized-key fingerprint already exists; admission cannot replace or reclassify it"
                    .to_string(),
            )
        } else {
            HandlerError::Internal(format!("{e:#}"))
        }
    })?;

    let response = Response {
        admitted: true,
        fingerprint,
        label,
        scopes,
        granted_by: format!(
            "admission:{}",
            token_hash.chars().take(12).collect::<String>()
        ),
        created_at,
        principal_class: token.class,
        origin_site_id: origin_site_id.map(str::to_owned),
        expires_at: super::grant_terms::expiry_text(terms.expires_at),
    };
    serde_json::to_value(response).map_err(|e| HandlerError::Internal(e.to_string()))
}

fn admission_token_path(app_root: &Path, token_hash: &str) -> PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("admission")
        .join("tokens")
        .join(format!("{token_hash}.toml"))
}

fn read_token_file(path: &Path) -> HandlerResult<AdmissionTokenFile> {
    let raw = std::fs::read_to_string(path).map_err(|_| {
        HandlerError::Forbidden("invalid or already-used admission token".to_string())
    })?;
    let stored: AdmissionTokenVersion = toml::from_str(&raw)
        .map_err(|e| HandlerError::BadRequest(format!("invalid token file: {e}")))?;
    if stored.version != ADMISSION_TOKEN_FORMAT_VERSION {
        return Err(refusal(
            "admission_token_stale",
            400,
            format!(
                "admission token is format version {}, but this node reads version {}; \
                 tokens are not converted, so mint a new one with `ryeos admission-token`",
                stored.version, ADMISSION_TOKEN_FORMAT_VERSION
            ),
        ));
    }
    toml::from_str(&raw).map_err(|e| HandlerError::BadRequest(format!("invalid token file: {e}")))
}

/// Check that the claim is the one the token admits and return the origin
/// site the grant is bound to: none for `local_client`, the claim's site for
/// `remote_node`, and the token's site for `remote_principal`.
fn check_claim_matches_token<'a>(
    req: &'a Request,
    token: &'a AdmissionTokenFile,
) -> HandlerResult<Option<&'a str>> {
    let has_cosignature = req.forwarding_key_id.is_some() || req.forwarding_signature.is_some();
    if req.principal_class != token.class {
        return Err(refusal(
            "admission_class_mismatch",
            403,
            format!(
                "this token admits class {}, but the claim asks for class {}; claim with \
                 principal_class {}, or mint a token for class {}",
                token.class.as_str(),
                req.principal_class.as_str(),
                token.class.as_str(),
                req.principal_class.as_str(),
            ),
        ));
    }
    if token.class != AuthorizedKeyPrincipalClass::RemotePrincipal && has_cosignature {
        return Err(refusal(
            "admission_class_mismatch",
            403,
            format!(
                "a {} claim carries no origin-node co-signature; only a remote_principal \
                 claim does, so drop the forwarding fields or mint a remote_principal token",
                token.class.as_str()
            ),
        ));
    }
    match token.class {
        AuthorizedKeyPrincipalClass::LocalClient => {
            if token.origin_site_id.is_some() || req.origin_site_id.is_some() {
                return Err(refusal(
                    "admission_origin_site_mismatch",
                    403,
                    "a local_client grant has no origin site; omit origin_site_id from the claim"
                        .to_string(),
                ));
            }
            Ok(None)
        }
        AuthorizedKeyPrincipalClass::RemoteNode => {
            if token.origin_site_id.is_some() {
                return Err(malformed_token(token.class));
            }
            let origin = req.origin_site_id.as_deref().ok_or_else(|| {
                HandlerError::BadRequest(
                    "a remote_node claim must carry its origin_site_id".to_string(),
                )
            })?;
            Ok(Some(origin))
        }
        AuthorizedKeyPrincipalClass::RemotePrincipal => {
            let bound = token
                .origin_site_id
                .as_deref()
                .ok_or_else(|| malformed_token(token.class))?;
            if req.origin_site_id.as_deref() != Some(bound) {
                return Err(refusal(
                    "admission_origin_site_mismatch",
                    403,
                    format!(
                        "this token is bound to origin site {bound}, but the claim names {}; \
                         claim through a node of {bound}, or mint a token for the claimant's site",
                        req.origin_site_id.as_deref().unwrap_or("no origin site"),
                    ),
                ));
            }
            Ok(Some(bound))
        }
    }
}

fn malformed_token(class: AuthorizedKeyPrincipalClass) -> HandlerError {
    refusal(
        "admission_token_invalid",
        400,
        format!(
            "the stored {} admission token has an inconsistent origin site; \
             mint a new one with `ryeos admission-token`",
            class.as_str()
        ),
    )
}

/// The hash the origin node co-signs for a `remote_principal` claim: the
/// forwarded-request construction applied to the claimant's signed claim, so
/// the node attests to this exact principal signature and no other.
pub fn cosignature_content_hash(
    audience: &str,
    claim_digest: &str,
    signed_at: u64,
    nonce: &str,
    principal_fingerprint: &str,
    claim_signature: &str,
    forwarding_key_id: &str,
    origin_site_id: &str,
) -> String {
    crate::auth::forwarding_request_content_hash(
        "POST",
        "/admission/claim",
        claim_digest,
        &signed_at.to_string(),
        nonce,
        audience,
        &format!("fp:{principal_fingerprint}"),
        claim_signature,
        forwarding_key_id,
        origin_site_id,
    )
}

/// Verify the origin node's co-signature on a `remote_principal` claim.
fn verify_origin_cosignature(
    req: &Request,
    claim_digest: &str,
    principal_fingerprint: &str,
    origin_site_id: &str,
    state: &AppState,
) -> HandlerResult<()> {
    let (Some(forwarding_key_id), Some(forwarding_signature)) = (
        req.forwarding_key_id.as_deref(),
        req.forwarding_signature.as_deref(),
    ) else {
        return Err(refusal(
            "admission_cosignature_missing",
            403,
            format!(
                "a remote_principal claim must carry the co-signature of an admitted node of \
                 {origin_site_id}; have that node co-sign the claim (forwarding_key_id and \
                 forwarding_signature) and claim again"
            ),
        ));
    };
    let content_hash = cosignature_content_hash(
        &state.identity.principal_id(),
        claim_digest,
        req.signed_at,
        &req.nonce,
        principal_fingerprint,
        &req.signature,
        forwarding_key_id,
        origin_site_id,
    );
    crate::auth::verify_source_node_cosignature(
        state,
        forwarding_key_id,
        origin_site_id,
        forwarding_signature,
        &content_hash,
    )
    .map(|_| ())
    .map_err(|reason| {
        refusal(
            "admission_cosignature_invalid",
            403,
            format!(
                "the origin-node co-signature is refused: {reason}; the co-signing node must be \
                 admitted here as a remote_node of {origin_site_id} holding the \
                 forwarded-operator attestation scope"
            ),
        )
    })
}

fn consume_token_file(path: &Path) -> HandlerResult<()> {
    let claimed = path.with_extension("claimed");
    std::fs::rename(path, &claimed).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            HandlerError::Forbidden("invalid or already-used admission token".to_string())
        } else {
            HandlerError::Internal(format!("failed to consume admission token: {e}"))
        }
    })
}

/// Undo `consume_token_file` after a failed grant write. A failed rename-back
/// is logged with the token path; the caller still returns the write error.
fn restore_token_file(path: &Path) {
    let claimed = path.with_extension("claimed");
    if let Err(error) = std::fs::rename(&claimed, path) {
        tracing::error!(
            token_path = %path.display(),
            claimed_path = %claimed.display(),
            %error,
            "admission grant write failed and the consumed token could not be restored"
        );
    }
}

fn hosted_admission_token_ttl_secs(state: &AppState) -> HandlerResult<u64> {
    let policy = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::hosted::HostedNodePolicy>()
        .map_err(|error| HandlerError::Internal(format!("load hosted-node policy: {error:#}")))?;
    match (policy.admission_enabled, policy.admission_token_ttl_secs) {
        (true, Some(ttl_secs)) => Ok(ttl_secs),
        (false, None) => Err(HandlerError::Forbidden(
            "hosted-node admission is disabled by node policy".to_string(),
        )),
        _ => Err(HandlerError::Internal(
            "hosted-node policy admission state is incoherent".to_string(),
        )),
    }
}

fn enforce_hosted_policy_token_ttl(
    maximum_token_ttl_secs: u64,
    token: &AdmissionTokenFile,
    now: u64,
) -> HandlerResult<()> {
    let issued_at_unix = token.issued_at_unix.ok_or_else(|| {
        HandlerError::Forbidden("hosted-node admission token is missing issued_at_unix".to_string())
    })?;
    let ttl_secs = token.ttl_secs.ok_or_else(|| {
        HandlerError::Forbidden("hosted-node admission token is missing ttl_secs".to_string())
    })?;
    if ttl_secs == 0 {
        return Err(HandlerError::Forbidden(
            "hosted-node admission token ttl_secs must be greater than zero".to_string(),
        ));
    }
    if issued_at_unix > now.saturating_add(CLAIM_MAX_AGE_SECS) {
        return Err(HandlerError::Forbidden(
            "hosted-node admission token issued_at_unix is in the future".to_string(),
        ));
    }
    if ttl_secs > maximum_token_ttl_secs {
        return Err(HandlerError::Forbidden(format!(
            "admission token TTL {}s exceeds hosted-node policy maximum {}s",
            ttl_secs, maximum_token_ttl_secs
        )));
    }
    let expected_expires_at_unix = issued_at_unix.checked_add(ttl_secs).ok_or_else(|| {
        HandlerError::Forbidden("hosted-node admission token expiry overflows".to_string())
    })?;
    if token.expires_at_unix > expected_expires_at_unix {
        return Err(HandlerError::Forbidden(
            "hosted-node admission token expires_at_unix exceeds issued_at_unix + ttl_secs"
                .to_string(),
        ));
    }
    Ok(())
}

fn parse_public_key(input: &str) -> HandlerResult<(String, VerifyingKey, String)> {
    let key_b64 = input.strip_prefix("ed25519:").ok_or_else(|| {
        HandlerError::BadRequest("public_key must be in 'ed25519:<base64>' format".into())
    })?;
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| HandlerError::BadRequest(format!("invalid base64 in public_key: {e}")))?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| HandlerError::BadRequest("ed25519 public key must be 32 bytes".into()))?;
    let verifying_key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| HandlerError::BadRequest(format!("invalid ed25519 public key: {e}")))?;
    let fingerprint = lillux::sha256_hex(&key_bytes);
    Ok((key_b64.to_string(), verifying_key, fingerprint))
}

fn normalize_scopes(scopes: &[String], context: &str) -> HandlerResult<Vec<String>> {
    if scopes.is_empty() {
        return Err(HandlerError::BadRequest("scopes must not be empty".into()));
    }
    let mut normalized: Vec<String> = scopes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    if normalized.len() != scopes.len() {
        return Err(HandlerError::BadRequest(
            "duplicate or empty scopes after normalization".into(),
        ));
    }
    for scope in &normalized {
        ryeos_runtime::authorizer::validate_scope_pattern(scope)
            .map_err(HandlerError::BadRequest)?;
    }
    if normalized.iter().any(|s| s.contains('*')) {
        return Err(HandlerError::Forbidden(format!(
            "wildcard scopes are forbidden for {context}"
        )));
    }
    Ok(normalized)
}

fn ensure_scope_subset(
    requested: &[String],
    allowed: &[String],
    state: &AppState,
) -> HandlerResult<()> {
    if allowed.is_empty() {
        return Err(HandlerError::Forbidden(
            "admission token grants no scopes".into(),
        ));
    }
    for scope in requested {
        let permitted = state
            .authorizer
            .authorize(
                allowed,
                &ryeos_runtime::authorizer::AuthorizationPolicy::require(scope.as_str()),
            )
            .is_ok();
        if !permitted {
            return Err(HandlerError::Forbidden(format!(
                "scope '{}' not permitted by admission token",
                scope
            )));
        }
    }
    Ok(())
}

fn verify_claim_signature(
    req: &Request,
    token_hash: &str,
    scopes: &[String],
    verifying_key: &VerifyingKey,
    state: &AppState,
) -> HandlerResult<String> {
    let claim = claim_string(
        &state.identity.principal_id(),
        token_hash,
        req.principal_class,
        &req.public_key,
        req.origin_site_id.as_deref(),
        scopes,
        req.signed_at,
        &req.nonce,
    );
    let content_hash = lillux::cas::sha256_hex(claim.as_bytes());
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(&req.signature)
        .map_err(|_| HandlerError::BadRequest("invalid signature encoding".into()))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| HandlerError::BadRequest("invalid signature".into()))?;
    verifying_key
        .verify(content_hash.as_bytes(), &signature)
        .map_err(|_| HandlerError::Forbidden("invalid admission claim signature".into()))?;
    Ok(content_hash)
}

/// The exact string a claimant signs.
pub fn claim_string(
    audience: &str,
    token_hash: &str,
    principal_class: AuthorizedKeyPrincipalClass,
    public_key: &str,
    origin_site_id: Option<&str>,
    scopes: &[String],
    signed_at: u64,
    nonce: &str,
) -> String {
    format!(
        "ryeos-admission-claim-v3\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        audience,
        token_hash,
        principal_class.as_str(),
        public_key,
        origin_site_id.unwrap_or(""),
        scopes.join(","),
        signed_at,
        nonce,
    )
}

fn token_hash(token: &str) -> String {
    lillux::cas::sha256_hex(token.as_bytes())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:admission/claim",
    endpoint: "admission.claim",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, _ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_scopes_rejects_exact_and_prefix_wildcards() {
        let exact = normalize_scopes(&["*".to_string()], "admission claim requests");
        assert!(matches!(exact, Err(HandlerError::Forbidden(_))));

        let prefix = normalize_scopes(
            &["ryeos.execute.service.*".to_string()],
            "admission token files",
        );
        assert!(matches!(prefix, Err(HandlerError::Forbidden(_))));
    }

    #[test]
    fn normalize_scopes_accepts_concrete_scopes() {
        let scopes = normalize_scopes(
            &[
                "ryeos.execute.service.objects/put".to_string(),
                "ryeos.execute.service.objects/has".to_string(),
            ],
            "admission claim requests",
        )
        .expect("concrete scopes should be accepted");

        assert_eq!(
            scopes,
            vec![
                "ryeos.execute.service.objects/has".to_string(),
                "ryeos.execute.service.objects/put".to_string(),
            ]
        );
    }
}
