//! Offline inspection of this node's accepted remote content testimonies
//! against its own activations, for `ryeos node doctor`.
//!
//! Read only and grant-free: signed heads and CAS objects are read from the
//! app root and verified under this node's public key. Bundles are never
//! resolved and nothing is admitted or repaired here. Launch and the
//! candidate-program emitter remain the authority; this only names in advance
//! the testimonies they would refuse, with the same fixes.

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;

use ryeos_state::external_content::products::qualification_testimony::{
    QualificationLane, REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE,
    RETIRED_REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACES, RemoteQualificationAcceptanceEvidence,
    RemoteQualificationAcceptanceSuperseded, RemoteQualificationTestimonyBody,
};
use ryeos_state::external_content::qualification_evidence::ContentQualificationEvidence;
use ryeos_state::external_content::qualification_purpose::QualificationSubject;
use ryeos_state::external_content::qualification_subject::ContentQualificationSubject;
use ryeos_state::objects::{
    Attestation, EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE, ExternalContentActivationReceipt,
};

/// Bound on any one inspected CAS object; acceptances and receipts are far
/// smaller.
const MAX_INSPECTED_OBJECT_BYTES: u64 = 4 * 1024 * 1024;

pub const USABLE: &str = "ok";
pub const NO_LOCAL_ACTIVATION: &str = "no_local_activation";
pub const LOCAL_ACTIVATION_DIFFERS: &str = "local_activation_differs";
pub const ACCEPTANCE_SUPERSEDED: &str = "acceptance_superseded";

/// One accepted testimony and what this node could do with it now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteContentTestimonyFinding {
    pub code: &'static str,
    pub lane: String,
    pub coordinate_id: String,
    pub acceptance_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_activation_program_digest: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub local_activation_program_digests: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RemoteContentTestimonyReport {
    pub findings: Vec<RemoteContentTestimonyFinding>,
}

impl RemoteContentTestimonyReport {
    /// Whether any accepted testimony needs the operator before it can be
    /// used.
    pub fn has_warnings(&self) -> bool {
        self.findings.iter().any(|finding| finding.code != USABLE)
    }
}

/// Inspect every acceptance this node signed. Content-lane acceptances are
/// matched against this node's own activation receipts; an acceptance in a
/// retired namespace (either lane) is listed with its cure.
pub fn inspect_remote_content_testimonies(app_root: &Path) -> Result<RemoteContentTestimonyReport> {
    let root = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf());
    let refs_root = root.state().join("refs");
    if !refs_root.is_dir() {
        return Ok(RemoteContentTestimonyReport::default());
    }
    let (fingerprint, node_key) =
        node_public_key(&root.node().join("identity/public-identity.json"))?;
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(fingerprint.clone(), node_key);
    let cas = lillux::CasStore::new(root.state().join("objects"));
    let load = |hash: &str| -> Result<Value> {
        cas.get_object_bounded(hash, MAX_INSPECTED_OBJECT_BYTES)?
            .with_context(|| format!("CAS object {hash} is absent"))
    };
    let own_heads = |namespace: &str| -> Result<Vec<ryeos_state::GenericHeadRef>> {
        Ok(
            ryeos_state::refs::list_verified_generic_head_refs(&refs_root, namespace, &trust)?
                .into_iter()
                .filter(|head| head.signer == fingerprint)
                .collect(),
        )
    };
    let load_acceptance = |hash: &str| -> Result<Attestation> {
        let attestation = Attestation::from_value(&load(hash)?)?;
        attestation.verify_with_key(&node_key)?;
        Ok(attestation)
    };

    let mut receipts = Vec::new();
    for head in own_heads(EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE)? {
        receipts.push(ExternalContentActivationReceipt::from_value(&load(
            &head.target_hash,
        )?)?);
    }

    let mut findings = Vec::new();
    for head in own_heads(REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE)? {
        let attestation = load_acceptance(&head.target_hash)?;
        let evidence =
            match RemoteQualificationAcceptanceEvidence::from_value(&attestation.evidence) {
                Ok(evidence) => evidence,
                Err(error) => {
                    if error
                        .downcast_ref::<RemoteQualificationAcceptanceSuperseded>()
                        .is_none()
                    {
                        return Err(error);
                    }
                    findings.push(superseded_finding(
                        head.name,
                        head.target_hash,
                        &attestation.evidence,
                    ));
                    continue;
                }
            };
        if evidence.lane != QualificationLane::Content {
            continue;
        }
        let body = RemoteQualificationTestimonyBody::from_value(&evidence.testimony.evidence)?;
        let qualification =
            ContentQualificationEvidence::from_attestation(&body.qualification_attestation)?;
        let QualificationSubject::ActivatedContent { content } = &qualification.purpose.subject
        else {
            bail!("accepted content testimony has no activated content subject");
        };
        let activation_ref = body
            .activation_ref
            .clone()
            .context("accepted content testimony names no activation")?;
        findings.push(classify_content_testimony(
            head.name,
            head.target_hash,
            activation_ref,
            content,
            &receipts,
        ));
    }
    for namespace in RETIRED_REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACES {
        for head in own_heads(namespace)? {
            // Classified from the outer record only: a superseded acceptance
            // is listed with its cure and never decoded as a current one.
            let attestation = load_acceptance(&head.target_hash)?;
            findings.push(superseded_finding(
                head.name,
                head.target_hash,
                &attestation.evidence,
            ));
        }
    }
    Ok(RemoteContentTestimonyReport { findings })
}

/// Whether this node holds an activation of exactly the content the origin
/// qualified: the same activation program digest and consumer, with a
/// component for the qualified declaration.
fn classify_content_testimony(
    coordinate_id: String,
    acceptance_hash: String,
    activation_ref: String,
    content: &ContentQualificationSubject,
    receipts: &[ExternalContentActivationReceipt],
) -> RemoteContentTestimonyFinding {
    let qualified = &content.activation_program_digest;
    let usable = receipts.iter().any(|receipt| {
        &receipt.activation_program_digest == qualified
            && receipt.consumer_ref == content.consumer_ref
            && receipt
                .components
                .iter()
                .any(|component| component.id == content.declaration_id)
    });
    let local = receipts
        .iter()
        .filter(|receipt| receipt.activation_ref == activation_ref)
        .map(|receipt| receipt.activation_program_digest.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let (code, fix) = if usable {
        (USABLE, None)
    } else if local.is_empty() {
        (
            NO_LOCAL_ACTIVATION,
            Some(format!(
                "activate the release the origin qualified (activation program {qualified}) on this node: `ryeos external-content activate {activation_ref} <online|offline> [offline-archive-root]`"
            )),
        )
    } else {
        (
            LOCAL_ACTIVATION_DIFFERS,
            Some(format!(
                "this node activated {activation_ref} as activation program {}, not the qualified {qualified}: install and activate the release the origin qualified, or re-qualify at the origin against this node's release and export that testimony",
                local.join(", ")
            )),
        )
    };
    RemoteContentTestimonyFinding {
        code,
        lane: QualificationLane::Content.name().to_owned(),
        coordinate_id,
        acceptance_hash,
        activation_ref: Some(activation_ref),
        qualified_activation_program_digest: Some(qualified.clone()),
        local_activation_program_digests: if usable { Vec::new() } else { local },
        fix,
    }
}

fn superseded_finding(
    coordinate_id: String,
    acceptance_hash: String,
    evidence: &Value,
) -> RemoteContentTestimonyFinding {
    let recorded_schema = evidence
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or("an unrecorded schema")
        .to_owned();
    RemoteContentTestimonyFinding {
        code: ACCEPTANCE_SUPERSEDED,
        lane: evidence
            .get("lane")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        coordinate_id,
        acceptance_hash,
        activation_ref: None,
        qualified_activation_program_digest: None,
        local_activation_program_digests: Vec::new(),
        fix: Some(
            RemoteQualificationAcceptanceSuperseded { recorded_schema }
                .fix()
                .to_owned(),
        ),
    }
}

/// This node's public key from its self-signed public identity document.
fn node_public_key(path: &Path) -> Result<(String, lillux::crypto::VerifyingKey)> {
    let identity = ryeos_node_runtime::identity::NodeIdentity::load_public_identity(path)?;
    let fingerprint = identity.verified_fingerprint()?;
    let bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(
            identity
                .signing_key
                .strip_prefix("ed25519:")
                .context("public node identity requires an ed25519 key")?,
        )?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid public node key length"))?;
    Ok((
        fingerprint,
        lillux::crypto::VerifyingKey::from_bytes(&bytes)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_state::objects::ExternalContentActivationComponentReceipt;

    fn subject() -> ContentQualificationSubject {
        let capsule =
            ryeos_state::external_execution::admission::test_support::content_candidate_capsule(
                &"a".repeat(64),
            )
            .unwrap();
        let purpose = capsule
            .retained_external_runtime_content_qualification
            .unwrap()
            .evidence
            .purpose;
        let QualificationSubject::ActivatedContent { content } = purpose.subject else {
            panic!("content fixture has no activated subject");
        };
        content
    }

    fn receipt(
        activation_ref: &str,
        program: &str,
        consumer: &str,
        component: &str,
    ) -> ExternalContentActivationReceipt {
        ExternalContentActivationReceipt::new(
            activation_ref.into(),
            program.into(),
            consumer.into(),
            "e".repeat(64),
            "f".repeat(64),
            "1".repeat(64),
            vec![ExternalContentActivationComponentReceipt {
                id: component.into(),
                binding_hash: "c".repeat(64),
            }],
            "9".repeat(64),
        )
        .unwrap()
    }

    fn classify(receipts: &[ExternalContentActivationReceipt]) -> RemoteContentTestimonyFinding {
        classify_content_testimony(
            "0".repeat(64),
            "8".repeat(64),
            "config:fixtures/runtime-activation".into(),
            &subject(),
            receipts,
        )
    }

    #[test]
    fn doctor_reports_content_testimonies_without_a_matching_local_activation() {
        let content = subject();
        let activation = "config:fixtures/runtime-activation";
        // This node activated exactly the qualified program: usable.
        let exact = receipt(
            activation,
            &content.activation_program_digest,
            &content.consumer_ref,
            &content.declaration_id,
        );
        let finding = classify(std::slice::from_ref(&exact));
        assert_eq!(finding.code, USABLE);
        assert!(finding.fix.is_none() && finding.local_activation_program_digests.is_empty());

        // Nothing activated at all.
        let finding = classify(&[]);
        assert_eq!(finding.code, NO_LOCAL_ACTIVATION);
        assert!(
            finding
                .fix
                .as_deref()
                .unwrap()
                .contains("ryeos external-content activate config:fixtures/runtime-activation")
        );

        // The same activation, but another release: both programs named.
        let other = "7".repeat(64);
        let finding = classify(&[receipt(
            activation,
            &other,
            &content.consumer_ref,
            &content.declaration_id,
        )]);
        assert_eq!(finding.code, LOCAL_ACTIVATION_DIFFERS);
        assert_eq!(
            finding.local_activation_program_digests,
            vec![other.clone()]
        );
        let fix = finding.fix.unwrap();
        assert!(fix.contains(&other) && fix.contains(&content.activation_program_digest));

        // The qualified program for another consumer or declaration is not
        // the qualified content.
        for receipt in [
            receipt(
                activation,
                &content.activation_program_digest,
                "tool:fixtures/other-consumer",
                &content.declaration_id,
            ),
            receipt(
                activation,
                &content.activation_program_digest,
                &content.consumer_ref,
                "other-runtime",
            ),
        ] {
            assert_ne!(classify(&[receipt]).code, USABLE);
        }

        // A superseded acceptance of either lane is named with its cure.
        for lane in ["content", "product"] {
            let finding = superseded_finding(
                "0".repeat(64),
                "8".repeat(64),
                &serde_json::json!({
                    "schema": "ryeos.remote_qualification_acceptance_evidence.v3",
                    "lane": lane,
                }),
            );
            assert_eq!(finding.code, ACCEPTANCE_SUPERSEDED);
            assert_eq!(finding.lane, lane);
            assert!(
                finding
                    .fix
                    .unwrap()
                    .contains("export-qualification-testimony")
            );
        }
        let report = RemoteContentTestimonyReport {
            findings: vec![classify(&[exact])],
        };
        assert!(!report.has_warnings());
    }

    #[test]
    fn an_uninitialized_app_root_has_nothing_to_inspect() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            inspect_remote_content_testimonies(root.path())
                .unwrap()
                .findings
                .is_empty()
        );
    }
}
