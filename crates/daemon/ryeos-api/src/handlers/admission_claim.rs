//! `admission/claim` — claim a one-time node-local admission token.
//!
//! This is deliberately small: it does not introduce a central auth root
//! or a new permission system. A provider/operator can place one-time
//! token files on the target node; the claimant proves possession of the
//! private key for the public key being admitted; the node then writes the
//! normal node-signed authorized-key grant.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use lillux::crypto::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
use crate::registry::ServiceDescriptor;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const CLAIM_MAX_AGE_SECS: u64 = 300;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Opaque one-time admission token delivered out-of-band by the
    /// target node operator or provider/provisioner.
    pub token: String,
    /// Ed25519 public key in `ed25519:<base64>` format. This is the key
    /// that future signed requests from the admitted node will use.
    pub public_key: String,
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
    /// string produced by [`claim_string`].
    pub signature: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionTokenFile {
    version: u32,
    token_hash: String,
    #[serde(default)]
    label: Option<String>,
    scopes: Vec<String>,
    expires_at_unix: u64,
}

#[derive(serde::Serialize)]
pub struct Response {
    pub admitted: bool,
    pub fingerprint: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub granted_by: String,
    pub created_at: String,
}

pub async fn handle(req: Request, state: Arc<AppState>) -> HandlerResult<Value> {
    let token_hash = token_hash(&req.token);
    let token_path = admission_token_path(&state.config.system_space_dir, &token_hash);
    let token = read_token_file(&token_path)?;

    if token.version != 1 {
        return Err(HandlerError::BadRequest(format!(
            "unsupported admission token version {}; expected 1",
            token.version
        )));
    }
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
    if now.abs_diff(req.signed_at) > CLAIM_MAX_AGE_SECS {
        return Err(HandlerError::Forbidden(
            "admission claim signature timestamp expired".to_string(),
        ));
    }
    if req.nonce.trim().is_empty() {
        return Err(HandlerError::BadRequest("nonce must not be empty".into()));
    }

    let (key_b64, verifying_key, fingerprint) = parse_public_key(&req.public_key)?;
    let scopes = normalize_scopes(&req.scopes, "admission claim requests")?;
    let allowed_scopes = normalize_scopes(&token.scopes, "admission token files")?;
    ensure_scope_subset(&scopes, &allowed_scopes, &state)?;
    verify_claim_signature(&req, &token_hash, &scopes, &verifying_key, &state)?;

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

    consume_token_file(&token_path)?;

    let created_at = lillux::time::iso8601_now();
    let auth_dir = state.config.authorized_keys_dir.clone();
    ryeos_app::identity::write_authorized_key_toml(
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
        state.identity.signing_key(),
        ryeos_app::identity::WildcardPolicy::Reject,
    )
    .map_err(|e| HandlerError::Internal(e.to_string()))?;

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
    };
    serde_json::to_value(response).map_err(|e| HandlerError::Internal(e.to_string()))
}

fn admission_token_path(system_space_dir: &Path, token_hash: &str) -> PathBuf {
    system_space_dir
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
    toml::from_str(&raw).map_err(|e| HandlerError::BadRequest(format!("invalid token file: {e}")))
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
) -> HandlerResult<()> {
    let claim = claim_string(
        &state.identity.principal_id(),
        token_hash,
        &req.public_key,
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
        .map_err(|_| HandlerError::Forbidden("invalid admission claim signature".into()))
}

fn claim_string(
    audience: &str,
    token_hash: &str,
    public_key: &str,
    scopes: &[String],
    signed_at: u64,
    nonce: &str,
) -> String {
    format!(
        "ryeos-admission-claim-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        audience,
        token_hash,
        public_key,
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
                "ryeos.execute.service.objects.put".to_string(),
                "ryeos.execute.service.objects.has".to_string(),
            ],
            "admission claim requests",
        )
        .expect("concrete scopes should be accepted");

        assert_eq!(
            scopes,
            vec![
                "ryeos.execute.service.objects.has".to_string(),
                "ryeos.execute.service.objects.put".to_string(),
            ]
        );
    }
}
