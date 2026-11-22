//! Signed remote-execute results.
//!
//! When a forwarding node asks for it (`attest_result: true` on `/execute`),
//! the executing node signs the result envelope it returns. The signature
//! binds three facts:
//!
//! - `request_digest`: the `ryeos-request-v1` content hash of the exact
//!   signed request that asked for this execution (method, path, body hash,
//!   timestamp, nonce and audience), so a result cannot be replayed onto a
//!   different request;
//! - `item_ref`: the canonical item ref the node executed;
//! - `result_digest`: the SHA-256 of the canonical JSON of the whole
//!   response envelope without its `result_attestation` member.
//!
//! The forwarding node verifies the attestation with the remote key pinned
//! by `remote configure` before it reads anything from the result. A result
//! without an attestation, or with one that does not verify, is refused.

use base64::Engine;
use lillux::crypto::{Signature, Signer, Verifier, VerifyingKey};
use serde_json::Value;

use ryeos_node_runtime::identity::NodeIdentity;

/// Response member that carries the attestation.
pub const ATTESTATION_FIELD: &str = "result_attestation";
/// Domain-separating schema of the signed statement.
pub const ATTESTATION_SCHEMA: &str = "ryeos.remote_execute_result.v1";

/// Why a remote result attestation was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResultAttestationError {
    /// The result carries no attestation.
    #[error("remote result carries no result_attestation")]
    Missing,
    /// The attestation is present but does not prove this result.
    #[error("remote result attestation refused: {0}")]
    Invalid(String),
}

fn result_digest(body: &Value) -> Result<String, ResultAttestationError> {
    let mut unsigned = body.clone();
    let object = unsigned
        .as_object_mut()
        .ok_or_else(|| ResultAttestationError::Invalid("result is not a JSON object".into()))?;
    object.remove(ATTESTATION_FIELD);
    let canonical = lillux::cas::canonical_json(&unsigned).map_err(|_| {
        ResultAttestationError::Invalid("result is not canonically encodable".into())
    })?;
    Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
}

fn statement_hash(
    signer: &str,
    request_digest: &str,
    item_ref: &str,
    result_digest: &str,
) -> Result<String, ResultAttestationError> {
    let statement = serde_json::json!({
        "schema": ATTESTATION_SCHEMA,
        "signer": signer,
        "request_digest": request_digest,
        "item_ref": item_ref,
        "result_digest": result_digest,
    });
    let canonical = lillux::cas::canonical_json(&statement).map_err(|_| {
        ResultAttestationError::Invalid("attestation statement is not encodable".into())
    })?;
    Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
}

/// Executing-node side: sign `body` for the request `request_digest` that
/// executed `item_ref`, and attach the attestation to `body`.
pub fn attest(
    body: &mut Value,
    identity: &NodeIdentity,
    request_digest: &str,
    item_ref: &str,
) -> Result<(), ResultAttestationError> {
    let signer = identity.principal_id();
    let result_digest = result_digest(body)?;
    let hash = statement_hash(&signer, request_digest, item_ref, &result_digest)?;
    let signature = Signer::sign(identity.signing_key(), hash.as_bytes());
    let attestation = serde_json::json!({
        "schema": ATTESTATION_SCHEMA,
        "signer": signer,
        "request_digest": request_digest,
        "item_ref": item_ref,
        "result_digest": result_digest,
        "signature": base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
    });
    body.as_object_mut()
        .ok_or_else(|| ResultAttestationError::Invalid("result is not a JSON object".into()))?
        .insert(ATTESTATION_FIELD.to_string(), attestation);
    Ok(())
}

/// Forwarding-node side: prove `body` was signed by the pinned remote key for
/// exactly the request and item this node sent.
pub fn verify(
    body: &Value,
    expected_signer: &str,
    pinned_key: &VerifyingKey,
    request_digest: &str,
    item_ref: &str,
) -> Result<(), ResultAttestationError> {
    let attestation = body
        .get(ATTESTATION_FIELD)
        .ok_or(ResultAttestationError::Missing)?
        .as_object()
        .ok_or_else(|| ResultAttestationError::Invalid("attestation is not an object".into()))?;
    fn string_field<'a>(
        attestation: &'a serde_json::Map<String, Value>,
        name: &str,
    ) -> Result<&'a str, ResultAttestationError> {
        attestation
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| ResultAttestationError::Invalid(format!("attestation omits {name}")))
    }
    let field = |name: &'static str| string_field(attestation, name);
    let schema = field("schema")?;
    if schema != ATTESTATION_SCHEMA {
        return Err(ResultAttestationError::Invalid(format!(
            "attestation schema '{schema}' is not '{ATTESTATION_SCHEMA}'"
        )));
    }
    let signer = field("signer")?;
    if signer != expected_signer {
        return Err(ResultAttestationError::Invalid(format!(
            "attestation signer '{signer}' is not the pinned remote '{expected_signer}'"
        )));
    }
    let attested_request = field("request_digest")?;
    if attested_request != request_digest {
        return Err(ResultAttestationError::Invalid(format!(
            "attestation binds request {attested_request}, not this request {request_digest}"
        )));
    }
    let attested_item = field("item_ref")?;
    if attested_item != item_ref {
        return Err(ResultAttestationError::Invalid(format!(
            "attestation binds item '{attested_item}', not the requested '{item_ref}'"
        )));
    }
    let attested_result = field("result_digest")?;
    let actual_result = result_digest(body)?;
    if attested_result != actual_result {
        return Err(ResultAttestationError::Invalid(
            "attestation result_digest does not match the returned result".into(),
        ));
    }
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(field("signature")?)
        .map_err(|_| ResultAttestationError::Invalid("signature is not base64".into()))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| ResultAttestationError::Invalid("signature is malformed".into()))?;
    let hash = statement_hash(signer, request_digest, item_ref, &actual_result)?;
    pinned_key.verify(hash.as_bytes(), &signature).map_err(|_| {
        ResultAttestationError::Invalid(
            "signature does not verify under the pinned remote key".into(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::{EncodePrivateKey, SigningKey};

    fn identity(seed: u8, dir: &std::path::Path) -> NodeIdentity {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let pem = key.to_pkcs8_pem(Default::default()).unwrap();
        let path = dir.join(format!("node-{seed}.pem"));
        std::fs::write(&path, pem.as_bytes()).unwrap();
        NodeIdentity::load(&path).unwrap()
    }

    const REQUEST: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const ITEM: &str = "tool:qualification/run";

    fn result() -> Value {
        serde_json::json!({
            "thread": {"thread_id": "T-1", "status": "completed"},
            "result": {"ok": true, "n": 1.5},
            "result_project_snapshot_hash": "a".repeat(64),
        })
    }

    fn signed(remote: &NodeIdentity) -> Value {
        let mut body = result();
        attest(&mut body, remote, REQUEST, ITEM).unwrap();
        body
    }

    #[test]
    fn signed_result_verifies_under_pinned_key() {
        let dir = tempfile::tempdir().unwrap();
        let remote = identity(7, dir.path());
        let body = signed(&remote);
        verify(
            &body,
            &remote.principal_id(),
            remote.verifying_key(),
            REQUEST,
            ITEM,
        )
        .unwrap();
    }

    #[test]
    fn signed_result_survives_a_json_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let remote = identity(7, dir.path());
        let wire = serde_json::to_vec(&signed(&remote)).unwrap();
        let body: Value = serde_json::from_slice(&wire).unwrap();
        verify(
            &body,
            &remote.principal_id(),
            remote.verifying_key(),
            REQUEST,
            ITEM,
        )
        .unwrap();
    }

    #[test]
    fn result_without_attestation_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let remote = identity(7, dir.path());
        assert_eq!(
            verify(
                &result(),
                &remote.principal_id(),
                remote.verifying_key(),
                REQUEST,
                ITEM
            ),
            Err(ResultAttestationError::Missing)
        );
    }

    #[test]
    fn result_signed_by_other_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let remote = identity(7, dir.path());
        let impostor = identity(8, dir.path());
        let body = signed(&impostor);
        // Wrong signer name.
        assert!(matches!(
            verify(
                &body,
                &remote.principal_id(),
                remote.verifying_key(),
                REQUEST,
                ITEM
            ),
            Err(ResultAttestationError::Invalid(_))
        ));
        // Impostor claims the pinned name but cannot sign as it.
        let mut forged = body.clone();
        forged[ATTESTATION_FIELD]["signer"] = Value::String(remote.principal_id());
        assert!(matches!(
            verify(
                &forged,
                &remote.principal_id(),
                remote.verifying_key(),
                REQUEST,
                ITEM
            ),
            Err(ResultAttestationError::Invalid(_))
        ));
    }

    #[test]
    fn tampered_result_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let remote = identity(7, dir.path());
        let mut body = signed(&remote);
        body["result_project_snapshot_hash"] = Value::String("b".repeat(64));
        assert!(matches!(
            verify(
                &body,
                &remote.principal_id(),
                remote.verifying_key(),
                REQUEST,
                ITEM
            ),
            Err(ResultAttestationError::Invalid(_))
        ));
    }

    #[test]
    fn result_for_another_request_or_item_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let remote = identity(7, dir.path());
        let body = signed(&remote);
        let other_request = "2".repeat(64);
        assert!(matches!(
            verify(
                &body,
                &remote.principal_id(),
                remote.verifying_key(),
                &other_request,
                ITEM
            ),
            Err(ResultAttestationError::Invalid(_))
        ));
        assert!(matches!(
            verify(
                &body,
                &remote.principal_id(),
                remote.verifying_key(),
                REQUEST,
                "tool:other/run"
            ),
            Err(ResultAttestationError::Invalid(_))
        ));
        // Rewriting the bound request in the attestation breaks the signature.
        let mut rebound = body.clone();
        rebound[ATTESTATION_FIELD]["request_digest"] = Value::String(other_request.clone());
        assert!(matches!(
            verify(
                &rebound,
                &remote.principal_id(),
                remote.verifying_key(),
                &other_request,
                ITEM
            ),
            Err(ResultAttestationError::Invalid(_))
        ));
    }
}
