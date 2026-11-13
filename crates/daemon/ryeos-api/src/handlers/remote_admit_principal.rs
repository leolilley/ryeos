//! `remote/admit-principal` — claim a `remote_principal` admission token on a
//! configured remote, as the calling principal.
//!
//! The principal signs the claim with its own key; this node never holds that
//! key. The node checks the signature under the caller's own grant, so a caller
//! can only admit itself, co-signs the claim as the origin site, and forwards it
//! to the remote's `/admission/claim`.

use std::sync::Arc;

use base64::Engine as _;
use lillux::crypto::{Signature, Signer as _, Verifier as _};
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::{HandlerError, HandlerResult};
use crate::handlers::admission_claim::{claim_string, cosignature_content_hash};
use crate::registry::ServiceDescriptor;
use crate::remote::client::{RemoteClient, delegator_fingerprint, normalize_admission_scopes};
use crate::remote::config;
use ryeos_app::handler_error::delegation_handler_error;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;

fn default_remote() -> String {
    "default".to_string()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// One-time `remote_principal` admission token minted on the remote.
    #[serde(default)]
    pub token: Option<String>,
    /// Optional label for the resulting grant on the remote.
    #[serde(default)]
    pub label: Option<String>,
    /// Capabilities requested, as the principal signed them.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Unix time the principal signed the claim.
    #[serde(default)]
    pub signed_at: Option<u64>,
    /// Nonce the principal signed into the claim.
    #[serde(default)]
    pub nonce: Option<String>,
    /// Base64 signature by the caller's key over the claim digest. Absent: the
    /// response carries the facts the caller needs to build the claim.
    #[serde(default)]
    pub signature: Option<String>,
}

/// The signed fields of a claim, as the principal presented them.
struct SignedClaim<'a> {
    token: &'a str,
    scopes: &'a [String],
    signed_at: u64,
    nonce: &'a str,
    signature: &'a str,
}

/// `ed25519:<base64>` of a verifying key, the form a claim names it in.
fn public_key_text(key: &lillux::crypto::VerifyingKey) -> String {
    format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(key.as_bytes())
    )
}

/// The claim body for `/admission/claim`: the principal's claim exactly as it
/// signed it, plus this node's co-signature as the origin site. Refuses a
/// signature the caller's own key did not make.
fn cosigned_claim_body(
    state: &AppState,
    fingerprint: &str,
    audience: &str,
    label: Option<&str>,
    claim: &SignedClaim<'_>,
) -> HandlerResult<Value> {
    let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
        fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
    .ok_or_else(|| HandlerError::Forbidden("the caller's grant is not on this node".into()))?;
    let public_key = public_key_text(&grant.public_key);
    let origin_site_id = state.threads.site_id().to_owned();
    let scopes = normalize_admission_scopes(claim.scopes)
        .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
    if scopes != claim.scopes {
        return Err(HandlerError::BadRequest(
            "scopes must be sorted and unique, exactly as they were signed".into(),
        ));
    }
    let claim_text = claim_string(
        audience,
        &lillux::cas::sha256_hex(claim.token.as_bytes()),
        AuthorizedKeyPrincipalClass::RemotePrincipal,
        &public_key,
        Some(&origin_site_id),
        &scopes,
        claim.signed_at,
        claim.nonce,
    );
    let claim_digest = lillux::cas::sha256_hex(claim_text.as_bytes());
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(claim.signature)
        .map_err(|_| HandlerError::BadRequest("invalid signature encoding".into()))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| HandlerError::BadRequest("invalid signature".into()))?;
    grant
        .public_key
        .verify(claim_digest.as_bytes(), &signature)
        .map_err(|_| {
            HandlerError::Forbidden(
                "the claim is not signed by the calling principal's key; a principal can only \
                 admit itself"
                    .into(),
            )
        })?;

    let key_id = state.identity.principal_id();
    let content_hash = cosignature_content_hash(
        audience,
        &claim_digest,
        claim.signed_at,
        claim.nonce,
        fingerprint,
        claim.signature,
        &key_id,
        &origin_site_id,
    );
    let cosignature = state.identity.signing_key().sign(content_hash.as_bytes());
    let mut body = json!({
        "protocol_version": 3,
        "token": claim.token,
        "public_key": public_key,
        "principal_class": AuthorizedKeyPrincipalClass::RemotePrincipal.as_str(),
        "origin_site_id": origin_site_id,
        "scopes": scopes,
        "signed_at": claim.signed_at,
        "nonce": claim.nonce,
        "signature": claim.signature,
        "forwarding_key_id": key_id,
        "forwarding_signature":
            base64::engine::general_purpose::STANDARD.encode(cosignature.to_bytes()),
    });
    if let Some(label) = label {
        body["label"] = Value::String(label.to_owned());
    }
    Ok(body)
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    ctx.require_verified()?;
    let fingerprint =
        delegator_fingerprint(&ctx).map_err(|error| delegation_handler_error(&error))?;
    // Admission tokens are credentials delivered by a target operator. Only
    // operator-owned remote configuration may select where one is released.
    let remotes = config::load_remotes(&state.config.app_root).map_err(|error| {
        config::typed_load_error_or(error, |error| {
            HandlerError::Internal(format!("load remotes: {error:#}"))
        })
    })?;
    let remote_cfg = config::get_remote(&remotes, &req.remote)
        .map_err(|error| HandlerError::BadRequest(format!("remote '{}': {error:#}", req.remote)))?;
    // The claim is forwarded to the remote, so an inbound-only pin has no path.
    remote_cfg.outbound_url()?;

    let grant = ryeos_node_runtime::identity::load_verified_authorized_key(
        &fingerprint,
        &state.config.authorized_keys_dir,
        &state.identity,
    )
    .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
    .ok_or_else(|| HandlerError::Forbidden("the caller's grant is not on this node".into()))?;
    let Some(signature) = req.signature.as_deref() else {
        return Ok(json!({
            "remote": remote_cfg.name,
            "template": {
                "audience": remote_cfg.principal_id,
                "origin_site_id": state.threads.site_id(),
                "public_key": public_key_text(&grant.public_key),
            },
        }));
    };
    let (Some(token), Some(signed_at), Some(nonce)) =
        (req.token.as_deref(), req.signed_at, req.nonce.as_deref())
    else {
        return Err(HandlerError::BadRequest(
            "a signed claim needs token, signed_at and nonce".into(),
        ));
    };
    let body = cosigned_claim_body(
        &state,
        &fingerprint,
        &remote_cfg.principal_id,
        req.label.as_deref(),
        &SignedClaim {
            token,
            scopes: &req.scopes,
            signed_at,
            nonce,
            signature,
        },
    )?;

    let client = RemoteClient::from_remote_cfg(&state, &remote_cfg)?;
    let live_identity = client
        .get_public_key()
        .await
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    live_identity
        .validate_configured_identity(&remote_cfg)
        .map_err(|error| HandlerError::BadRequest(format!("{error:#}")))?;
    let receipt = client
        .post_admission_claim(&body)
        .await
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    Ok(json!({
        "remote": remote_cfg.name,
        "url": remote_cfg.url,
        "origin_site_id": state.threads.site_id(),
        "claim": receipt,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/admit-principal",
    endpoint: "remote.admit-principal",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admit-principal"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_test_state;
    use lillux::crypto::SigningKey;

    fn principal(state: &AppState, key: &SigningKey) -> String {
        let vk = key.verifying_key();
        let fingerprint = lillux::signature::compute_fingerprint(&vk);
        ryeos_node_runtime::identity::write_authorized_key_toml(
            &state.config.authorized_keys_dir,
            &fingerprint,
            &base64::engine::general_purpose::STANDARD.encode(vk.as_bytes()),
            &["ryeos.execute.service.remote/admit-principal".to_string()],
            "test-principal",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            state.identity.signing_key(),
            ryeos_node_runtime::identity::WildcardPolicy::Reject,
        )
        .unwrap();
        fingerprint
    }

    /// Admit this node's own key as a `remote_node` of its own site holding the
    /// attestation scope, so its co-signature can be verified in-process.
    fn admit_self_as_origin_node(state: &AppState) {
        let vk = state.identity.verifying_key();
        ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
            &state.config.authorized_keys_dir,
            &lillux::signature::compute_fingerprint(&vk),
            &base64::engine::general_purpose::STANDARD.encode(vk.as_bytes()),
            &[ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE.to_string()],
            "origin-node",
            "test-granter",
            "2026-01-01T00:00:00Z",
            &ryeos_node_runtime::identity::GrantTerms::for_tests(),
            state.threads.site_id(),
            state.identity.signing_key(),
        )
        .unwrap();
    }

    fn signed_claim(
        state: &AppState,
        key: &SigningKey,
        audience: &str,
        scopes: &[String],
    ) -> (String, u64, String) {
        let public_key = public_key_text(&key.verifying_key());
        let (signed_at, nonce) = (lillux::time::unix_secs_now(), "claim-nonce".to_string());
        let text = claim_string(
            audience,
            &lillux::cas::sha256_hex(b"principal-token"),
            AuthorizedKeyPrincipalClass::RemotePrincipal,
            &public_key,
            Some(state.threads.site_id()),
            scopes,
            signed_at,
            &nonce,
        );
        let digest = lillux::cas::sha256_hex(text.as_bytes());
        let signature = key.sign(digest.as_bytes());
        (
            base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            signed_at,
            nonce,
        )
    }

    #[test]
    fn signer_must_equal_the_caller_and_the_cosignature_verifies() {
        let (_tmp, _owner, state) = build_test_state();
        admit_self_as_origin_node(&state);
        let caller = SigningKey::from_bytes(&[41u8; 32]);
        let other = SigningKey::from_bytes(&[42u8; 32]);
        let fingerprint = principal(&state, &caller);
        let audience = "fp:remote-node-audience";
        let scopes = vec!["ryeos.execute.service.remote/run".to_string()];

        // A claim signed by another key is refused: a caller admits only itself.
        let (signature, signed_at, nonce) = signed_claim(&state, &other, audience, &scopes);
        let forged = cosigned_claim_body(
            &state,
            &fingerprint,
            audience,
            None,
            &SignedClaim {
                token: "principal-token",
                scopes: &scopes,
                signed_at,
                nonce: &nonce,
                signature: &signature,
            },
        );
        assert!(matches!(forged, Err(HandlerError::Forbidden(_))));

        // The caller's own claim is co-signed, and the co-signature verifies
        // under the same rule the admitting node applies.
        let (signature, signed_at, nonce) = signed_claim(&state, &caller, audience, &scopes);
        let body = cosigned_claim_body(
            &state,
            &fingerprint,
            audience,
            Some("laptop"),
            &SignedClaim {
                token: "principal-token",
                scopes: &scopes,
                signed_at,
                nonce: &nonce,
                signature: &signature,
            },
        )
        .expect("the caller's own claim is co-signed");
        assert_eq!(body["principal_class"], "remote_principal");
        assert_eq!(body["origin_site_id"], state.threads.site_id());
        let key_id = body["forwarding_key_id"].as_str().unwrap();
        let claim_digest = lillux::cas::sha256_hex(
            claim_string(
                audience,
                &lillux::cas::sha256_hex(b"principal-token"),
                AuthorizedKeyPrincipalClass::RemotePrincipal,
                body["public_key"].as_str().unwrap(),
                Some(state.threads.site_id()),
                &scopes,
                signed_at,
                &nonce,
            )
            .as_bytes(),
        );
        let content_hash = cosignature_content_hash(
            audience,
            &claim_digest,
            signed_at,
            &nonce,
            &fingerprint,
            &signature,
            key_id,
            state.threads.site_id(),
        );
        crate::auth::verify_source_node_cosignature(
            &state,
            key_id,
            state.threads.site_id(),
            body["forwarding_signature"].as_str().unwrap(),
            &content_hash,
        )
        .expect("the co-signature verifies as the origin node's");
    }

    #[test]
    fn unsorted_scopes_are_refused_before_signing() {
        let (_tmp, _owner, state) = build_test_state();
        let caller = SigningKey::from_bytes(&[43u8; 32]);
        let fingerprint = principal(&state, &caller);
        let scopes = vec![
            "ryeos.execute.service.remote/run".to_string(),
            "ryeos.execute.service.objects/has".to_string(),
        ];
        let (signature, signed_at, nonce) = signed_claim(&state, &caller, "fp:audience", &scopes);
        let refused = cosigned_claim_body(
            &state,
            &fingerprint,
            "fp:audience",
            None,
            &SignedClaim {
                token: "principal-token",
                scopes: &scopes,
                signed_at,
                nonce: &nonce,
                signature: &signature,
            },
        );
        assert!(matches!(refused, Err(HandlerError::BadRequest(_))));
    }
}
