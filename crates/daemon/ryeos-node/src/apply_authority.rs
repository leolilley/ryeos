//! How a document the node writes proves that the operator authorized it.
//!
//! The operator's private key lives where the operator works. A document the
//! daemon writes while applying a queued offline operation is therefore
//! signed by the node key, and carries the digest of the operator-signed
//! request that authorized it together with that request. A verifier accepts
//! the document as the operator's authority only through that request: the
//! node signature proves who wrote the bytes, the request's operator
//! signature (checked against the operator public identity or the signer
//! registry's operator lineage) proves the operator asked for the change.
//!
//! Three forms exist, named by [`Authority`]:
//!
//! - `operator`: signed by the operator key itself, in the operator's own
//!   process (the initialization ceremony, the command-line tool).
//! - `node_under_request`: signed by the node key, authorized by an
//!   operator-signed request embedded beside the body.
//! - `node`: signed by the node key alone, for a node's own reseal after a
//!   change an authenticated live caller made; it names no request. Only
//!   documents that say so accept it.

use anyhow::{Context, Result, bail, ensure};
use lillux::crypto::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::offline_operation::contract::{SignedRequest, decode_signature, encode_signature};

/// Which key signed a document and what vouches for the operator's consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signed_by", rename_all = "snake_case")]
pub enum Authority {
    Operator,
    NodeUnderRequest { request_digest: String },
    Node,
}

/// The key a producer signs a document with.
#[derive(Clone, Copy)]
pub enum DocumentSigner<'a> {
    /// The operator's own key, in the operator's own process.
    Operator(&'a SigningKey),
    /// The node key, under the operator-signed request that authorized the
    /// change.
    Request {
        node: &'a SigningKey,
        request: &'a SignedRequest,
    },
    /// The node key alone.
    Node(&'a SigningKey),
}

impl DocumentSigner<'_> {
    /// The authority a body signed this way must carry.
    pub fn authority(&self) -> Result<Authority> {
        Ok(match self {
            Self::Operator(_) => Authority::Operator,
            Self::Request { request, .. } => Authority::NodeUnderRequest {
                request_digest: request.digest()?,
            },
            Self::Node(_) => Authority::Node,
        })
    }

    /// The request to embed beside the body, when there is one.
    pub fn request(&self) -> Option<SignedRequest> {
        match self {
            Self::Request { request, .. } => Some((*request).clone()),
            _ => None,
        }
    }

    /// `ed25519:<base64>` over `canonical`.
    pub fn sign(&self, canonical: &[u8]) -> String {
        let key = match self {
            Self::Operator(key) | Self::Node(key) => key,
            Self::Request { node, .. } => node,
        };
        encode_signature(&key.sign(canonical))
    }

    /// The fingerprint of the operator whose consent this document records:
    /// the operator key's own, or the request signer's. A node-alone
    /// signature records none.
    pub fn operator_fingerprint(&self) -> Option<String> {
        match self {
            Self::Operator(key) => Some(ryeos_engine::trust::compute_fingerprint(
                &key.verifying_key(),
            )),
            Self::Request { request, .. } => Some(request.body.signer_fingerprint.clone()),
            Self::Node(_) => None,
        }
    }
}

/// What a verifier knows: who the node is, how to find an operator's public
/// key, and which operations a request may name for this kind of document.
pub struct Acceptance<'a> {
    pub node_key: &'a VerifyingKey,
    /// The operator key for a fingerprint, or why that operator is refused.
    pub operator_key: &'a dyn Fn(&str) -> Result<VerifyingKey>,
    /// Operations a request authorizing this document may name.
    pub operations: &'a [&'a str],
    /// Whether the node-alone form is accepted for this document.
    pub allow_node_alone: bool,
    /// The operator fingerprint the body names; an operator-signed document
    /// must verify under that operator.
    pub claimed_operator: &'a str,
}

/// What a verified document's authority established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Established {
    /// The operator whose consent the document records; absent for the
    /// node-alone form.
    pub operator_fingerprint: Option<String>,
}

/// Verify `signature` over `canonical` and the authority the body names.
/// Every refusal carries a stable code.
pub fn verify(
    canonical: &[u8],
    signature: &str,
    authority: &Authority,
    request: Option<&SignedRequest>,
    accept: &Acceptance<'_>,
) -> Result<Established> {
    let signature = decode_signature(signature)
        .context("apply_authority_signature_invalid: the document signature is not readable")?;
    match authority {
        Authority::Operator => {
            ensure!(
                request.is_none(),
                "apply_authority_unexpected_request: an operator-signed document carries no request"
            );
            let key = (accept.operator_key)(accept.claimed_operator)?;
            key.verify(canonical, &signature).context(
                "apply_authority_signature_invalid: the operator signature does not verify",
            )?;
            Ok(Established {
                operator_fingerprint: Some(accept.claimed_operator.to_owned()),
            })
        }
        Authority::Node => {
            ensure!(
                accept.allow_node_alone,
                "apply_authority_request_required: this document must be signed by the operator \
                 or by the node under an operator-signed request, and it is signed by the node \
                 alone"
            );
            ensure!(
                request.is_none(),
                "apply_authority_unexpected_request: a node-signed document that names no \
                 request carries none"
            );
            accept
                .node_key
                .verify(canonical, &signature)
                .context("apply_authority_signature_invalid: the node signature does not verify")?;
            Ok(Established {
                operator_fingerprint: None,
            })
        }
        Authority::NodeUnderRequest { request_digest } => {
            let request = request.context(
                "apply_authority_request_missing: the document names an authorizing request and \
                 carries none",
            )?;
            accept
                .node_key
                .verify(canonical, &signature)
                .context("apply_authority_signature_invalid: the node signature does not verify")?;
            ensure!(
                request.digest()? == *request_digest,
                "apply_authority_request_digest: the embedded request is not the one the \
                 document names"
            );
            ensure!(
                request.body.node_fingerprint
                    == ryeos_engine::trust::compute_fingerprint(accept.node_key),
                "apply_authority_request_node: the authorizing request is for another node"
            );
            ensure!(
                request.body.signer_grant_digest.is_none(),
                "apply_authority_request_not_operator: the authorizing request is signed under \
                 a delegated grant, not by the node's operator"
            );
            if !accept.operations.contains(&request.body.operation.as_str()) {
                bail!(
                    "apply_authority_request_operation: the authorizing request names operation \
                     `{}`, which does not authorize this document",
                    request.body.operation
                );
            }
            let signer = &request.body.signer_fingerprint;
            let key = (accept.operator_key)(signer).with_context(|| {
                format!(
                    "apply_authority_request_not_operator: the authorizing request is signed by \
                     {signer}, which is not this node's operator"
                )
            })?;
            request.verify_signature(&key).context(
                "apply_authority_request_not_operator: the authorizing request is not signed by \
                 the operator it names",
            )?;
            Ok(Established {
                operator_fingerprint: Some(signer.clone()),
            })
        }
    }
}

/// A request for `node`, signed by `signer`, as unit controls need one.
#[cfg(test)]
pub(crate) fn test_request(
    node: &SigningKey,
    signer: &SigningKey,
    operation: &str,
) -> SignedRequest {
    use crate::offline_operation::contract::{REQUEST_SCHEMA, RequestBody};
    SignedRequest::sign(
        RequestBody {
            schema: REQUEST_SCHEMA.to_owned(),
            node_fingerprint: ryeos_engine::trust::compute_fingerprint(&node.verifying_key()),
            signer_fingerprint: ryeos_engine::trust::compute_fingerprint(&signer.verifying_key()),
            signer_grant_digest: None,
            signer_origin_site_id: None,
            operation: operation.to_owned(),
            arguments: serde_json::json!({}),
            inputs: std::collections::BTreeMap::new(),
            expected_result: std::collections::BTreeMap::new(),
            precondition: std::collections::BTreeMap::new(),
            issued_at: "2026-01-01T00:00:00Z".to_owned(),
            not_after: "2026-01-01T01:00:00Z".to_owned(),
        },
        signer,
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn fp(key: &SigningKey) -> String {
        ryeos_engine::trust::compute_fingerprint(&key.verifying_key())
    }

    fn request(node: &SigningKey, signer: &SigningKey, operation: &str) -> SignedRequest {
        super::test_request(node, signer, operation)
    }

    fn acceptance<'a>(
        node: &'a VerifyingKey,
        operator: &'a dyn Fn(&str) -> Result<VerifyingKey>,
        claimed: &'a str,
    ) -> Acceptance<'a> {
        Acceptance {
            node_key: node,
            operator_key: operator,
            operations: &["node.capacity.reissue"],
            allow_node_alone: false,
            claimed_operator: claimed,
        }
    }

    fn sign_under(
        node: &SigningKey,
        request: &SignedRequest,
    ) -> (Vec<u8>, String, Authority, Option<SignedRequest>) {
        let signer = DocumentSigner::Request { node, request };
        let canonical = b"body".to_vec();
        let signature = signer.sign(&canonical);
        (
            canonical,
            signature,
            signer.authority().unwrap(),
            signer.request(),
        )
    }

    #[test]
    fn a_node_signed_document_under_an_operator_request_verifies() {
        let (node, operator) = (key(1), key(2));
        let lookup = |f: &str| -> Result<VerifyingKey> {
            if f == fp(&operator) {
                Ok(operator.verifying_key())
            } else {
                bail!("unknown operator")
            }
        };
        let request = request(&node, &operator, "node.capacity.reissue");
        let (canonical, signature, authority, embedded) = sign_under(&node, &request);
        assert_eq!(
            authority,
            Authority::NodeUnderRequest {
                request_digest: request.digest().unwrap()
            }
        );
        let node_public = node.verifying_key();
        let claimed = fp(&operator);
        let established = verify(
            &canonical,
            &signature,
            &authority,
            embedded.as_ref(),
            &acceptance(&node_public, &lookup, &claimed),
        )
        .unwrap();
        assert_eq!(established.operator_fingerprint, Some(claimed));
    }

    #[test]
    fn a_request_that_the_operator_did_not_sign_is_refused() {
        let (node, operator, stranger) = (key(1), key(2), key(3));
        let lookup = |f: &str| -> Result<VerifyingKey> {
            if f == fp(&operator) {
                Ok(operator.verifying_key())
            } else {
                bail!("unknown operator")
            }
        };
        let node_public = node.verifying_key();
        let claimed = fp(&operator);
        // Signed by a key the node does not know as its operator.
        let by_stranger = request(&node, &stranger, "node.capacity.reissue");
        let (canonical, signature, authority, embedded) = sign_under(&node, &by_stranger);
        let error = verify(
            &canonical,
            &signature,
            &authority,
            embedded.as_ref(),
            &acceptance(&node_public, &lookup, &claimed),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("apply_authority_request_not_operator"));
        // Names the operator but carries another key's signature.
        let mut forged = request(&node, &operator, "node.capacity.reissue");
        forged.signature = request(&node, &stranger, "node.capacity.reissue").signature;
        let (canonical, signature, authority, embedded) = sign_under(&node, &forged);
        let error = verify(
            &canonical,
            &signature,
            &authority,
            embedded.as_ref(),
            &acceptance(&node_public, &lookup, &claimed),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("apply_authority_request_not_operator"));
    }

    #[test]
    fn the_request_must_be_present_name_this_node_and_this_operation() {
        let (node, operator, other_node) = (key(1), key(2), key(4));
        let lookup = |f: &str| -> Result<VerifyingKey> {
            assert_eq!(f, fp(&operator));
            Ok(operator.verifying_key())
        };
        let node_public = node.verifying_key();
        let claimed = fp(&operator);
        let good = request(&node, &operator, "node.capacity.reissue");
        let (canonical, signature, authority, _) = sign_under(&node, &good);
        let missing = verify(
            &canonical,
            &signature,
            &authority,
            None,
            &acceptance(&node_public, &lookup, &claimed),
        )
        .unwrap_err();
        assert!(format!("{missing:#}").contains("apply_authority_request_missing"));
        for (request, code) in [
            (
                request(&other_node, &operator, "node.capacity.reissue"),
                "apply_authority_request_node",
            ),
            (
                request(&node, &operator, "node.allow-restart"),
                "apply_authority_request_operation",
            ),
        ] {
            let (canonical, signature, authority, embedded) = sign_under(&node, &request);
            let error = verify(
                &canonical,
                &signature,
                &authority,
                embedded.as_ref(),
                &acceptance(&node_public, &lookup, &claimed),
            )
            .unwrap_err();
            assert!(format!("{error:#}").contains(code), "{code}: {error:#}");
        }
        // A request carried over from another document does not match the
        // digest the body names.
        let other = request(&node, &operator, "node.capacity.reissue");
        let (canonical, signature, authority, _) = sign_under(&node, &good);
        let mut swapped = other;
        swapped.body.expected_result.insert("x".into(), "y".into());
        let error = verify(
            &canonical,
            &signature,
            &authority,
            Some(&swapped),
            &acceptance(&node_public, &lookup, &claimed),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("apply_authority_request_digest"));
    }

    #[test]
    fn the_node_alone_form_is_refused_unless_the_document_accepts_it() {
        let node = key(1);
        let lookup = |_: &str| -> Result<VerifyingKey> { bail!("no operators") };
        let node_public = node.verifying_key();
        let signer = DocumentSigner::Node(&node);
        let signature = signer.sign(b"body");
        let authority = signer.authority().unwrap();
        let mut accept = acceptance(&node_public, &lookup, "x");
        assert!(
            verify(b"body", &signature, &authority, None, &accept)
                .unwrap_err()
                .to_string()
                .contains("apply_authority_request_required")
        );
        accept.allow_node_alone = true;
        assert_eq!(
            verify(b"body", &signature, &authority, None, &accept)
                .unwrap()
                .operator_fingerprint,
            None
        );
    }
}
