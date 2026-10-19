//! Composition of one node-signed runtime snapshot qualification binding.
//!
//! The binding is anchored to an installed production binding: its id and
//! digest are read back from the verified document, and the qualification
//! settings are the producer's own settings, so the restored Sandbox is
//! created in the exact provider scope the snapshot was produced in. The
//! operator adds the provider spec, the restored verifier artifact and the
//! optional consumer verifier inventory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ryeos_external_execution_contract::restored_runtime_measurement::ConsumerRuntimeVerifierSelection;
use serde::Serialize;

use super::{QualificationDocument, RuntimeSnapshotQualificationSection, SECTION_NAME};
use crate::node_config::NodeConfigSection as _;
use crate::node_config::sections::external_execution::compose::{
    item_context, load_node_identity, publish_signed_document, sign_node_document,
    validate_document_id, verify_node_signed,
};
use crate::node_config::sections::runtime_snapshot_production::compose::installed_production_binding;

/// Largest provider spec the lifecycle adapter accepts.
pub const MAX_PROVIDER_SPEC_BYTES: usize = 64 * 1024;

/// Protocol of the qualification document this composer emits.
const QUALIFICATION_PROTOCOL: &str = "ryeos.runtime-snapshot-qualification.v3";

/// Budgets of one qualification attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationLimits {
    pub contact_timeout_seconds: u32,
    pub maximum_lifetime_seconds: u32,
}

impl Default for QualificationLimits {
    fn default() -> Self {
        Self {
            contact_timeout_seconds: 60,
            maximum_lifetime_seconds: 900,
        }
    }
}

/// One operator request to compose and install a qualification binding.
pub struct QualificationComposeRequest<'a> {
    pub app_root: &'a Path,
    /// Proof that the node is stopped; no daemon may read the section while
    /// it is rewritten.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLock,
    pub id: String,
    /// Installed production binding this qualification attempt restores.
    pub snapshot_binding_id: String,
    /// Bytes of the lifecycle provider spec the installed adapter carries.
    pub provider_spec: Vec<u8>,
    /// Content hash of the restored-runtime verifier executable.
    pub verifier_artifact_hash: String,
    /// Consumer verifier selections keyed by scenario id, as a JSON object;
    /// absent means none.
    pub consumer_verifiers: Option<serde_json::Value>,
    pub limits: QualificationLimits,
}

/// What was installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QualificationComposeReport {
    pub id: String,
    pub path: PathBuf,
    pub production_binding_id: String,
    pub production_binding_digest: String,
    pub provider_spec_digest: String,
    pub verifier_artifact_hash: String,
    pub settings_digest: String,
    pub binding_digest: String,
}

/// Compose, sign with the node identity, verify the exact bytes the loader
/// will read, and publish atomically.
pub fn compose_runtime_snapshot_qualification_binding(
    request: QualificationComposeRequest<'_>,
) -> Result<QualificationComposeReport> {
    request
        .state_lock
        .ensure_protects_app_root(request.app_root)?;
    validate_document_id(&request.id, "runtime snapshot qualification binding")?;
    ensure!(
        !request.provider_spec.is_empty() && request.provider_spec.len() <= MAX_PROVIDER_SPEC_BYTES,
        "provider spec must be 1..={MAX_PROVIDER_SPEC_BYTES} bytes"
    );
    ensure!(
        serde_json::from_slice::<serde_json::Value>(&request.provider_spec)
            .context("provider spec is not JSON")?
            .is_object(),
        "provider spec must be a JSON object"
    );
    let consumer_verifiers: BTreeMap<String, ConsumerRuntimeVerifierSelection> =
        match request.consumer_verifiers {
            Some(value) => serde_json::from_value(value).context("invalid consumer verifiers")?,
            None => BTreeMap::new(),
        };
    let identity = load_node_identity(request.app_root)?;
    let producer =
        installed_production_binding(request.app_root, &request.snapshot_binding_id, &identity)?;
    let settings = producer.settings().clone();
    let document = QualificationDocument {
        kind: "node".into(),
        schema: 3,
        protocol: QUALIFICATION_PROTOCOL.into(),
        production_binding_id: producer.id().to_owned(),
        production_binding_digest: producer.digest().to_owned(),
        provider_spec_digest: lillux::sha256_hex(&request.provider_spec),
        settings_digest: producer.settings_digest().to_owned(),
        settings,
        verifier_artifact_hash: request.verifier_artifact_hash,
        consumer_verifiers,
        contact_timeout_seconds: request.limits.contact_timeout_seconds,
        maximum_lifetime_seconds: request.limits.maximum_lifetime_seconds,
    };
    document.validate()?;
    let signed = sign_node_document(&serde_yaml::to_string(&document)?, &identity)?;
    let body = verify_node_signed(&signed, &identity, "runtime snapshot qualification binding")?;
    // The section parse is the loader's own gate for this document.
    RuntimeSnapshotQualificationSection.parse(
        &item_context(
            request.app_root,
            SECTION_NAME,
            &request.id,
            identity.fingerprint(),
        ),
        &body,
    )?;
    let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.runtime-snapshot-qualification-binding.v1",
        "id": request.id,
        "signer": identity.fingerprint(),
        "signed_source": lillux::sha256_hex(signed.as_bytes()),
    }))?;
    let path = publish_signed_document(
        request.app_root,
        SECTION_NAME,
        &request.id,
        signed.as_bytes(),
    )?;
    Ok(QualificationComposeReport {
        id: request.id,
        path,
        production_binding_id: document.production_binding_id,
        production_binding_digest: document.production_binding_digest,
        provider_spec_digest: document.provider_spec_digest,
        verifier_artifact_hash: document.verifier_artifact_hash,
        settings_digest: document.settings_digest,
        binding_digest: digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_config::sections::runtime_snapshot_production::compose::tests::{
        StoppedNode, install_production, stopped_node,
    };

    const PROVIDER_SPEC: &[u8] = br#"{"schema":1}"#;

    fn request<'a>(node: &'a StoppedNode, id: &str) -> QualificationComposeRequest<'a> {
        QualificationComposeRequest {
            app_root: node.root.path(),
            state_lock: &node.lock,
            id: id.into(),
            snapshot_binding_id: "snapshot-producer".into(),
            provider_spec: PROVIDER_SPEC.to_vec(),
            verifier_artifact_hash: "3".repeat(64),
            consumer_verifiers: None,
            limits: QualificationLimits::default(),
        }
    }

    #[test]
    fn installs_a_document_bound_to_the_installed_producer() {
        let node = stopped_node();
        let producer = install_production(&node, "snapshot-producer");
        let report =
            compose_runtime_snapshot_qualification_binding(request(&node, "qualifier")).unwrap();
        assert_eq!(report.production_binding_id, "snapshot-producer");
        assert_eq!(report.production_binding_digest, producer.binding_digest);
        assert_eq!(report.settings_digest, producer.settings_digest);
        assert_eq!(
            report.provider_spec_digest,
            lillux::sha256_hex(PROVIDER_SPEC)
        );
        assert!(
            report
                .path
                .ends_with(".ai/node/runtime_snapshot_qualification/qualifier.yaml")
        );
        let signed = std::fs::read_to_string(&report.path).unwrap();
        let identity = load_node_identity(node.root.path()).unwrap();
        let body = verify_node_signed(&signed, &identity, "binding").unwrap();
        let document: QualificationDocument = serde_json::from_value(body).unwrap();
        document.validate().unwrap();
        assert_eq!(document.settings["sandbox_group_id"], "sbg-exact");
    }

    #[test]
    fn an_absent_producer_is_refused_and_publishes_nothing() {
        let node = stopped_node();
        let error = compose_runtime_snapshot_qualification_binding(request(&node, "qualifier"))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("is not installed"),
            "{error:#}"
        );
        assert!(
            !node
                .root
                .path()
                .join(".ai/node/runtime_snapshot_qualification")
                .exists()
        );
    }

    #[test]
    fn a_lock_that_does_not_protect_the_app_root_is_refused() {
        let node = stopped_node();
        install_production(&node, "snapshot-producer");
        let elsewhere = tempfile::tempdir().unwrap();
        let other = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(elsewhere.path()),
        )
        .unwrap();
        let mut req = request(&node, "qualifier");
        req.state_lock = &other;
        assert!(compose_runtime_snapshot_qualification_binding(req).is_err());
        assert!(
            !node
                .root
                .path()
                .join(".ai/node/runtime_snapshot_qualification")
                .exists()
        );
    }

    #[test]
    fn invalid_fields_are_refused_and_publish_nothing() {
        let node = stopped_node();
        install_production(&node, "snapshot-producer");
        let refused = |mutate: &dyn Fn(&mut QualificationComposeRequest<'_>)| {
            let mut req = request(&node, "qualifier");
            mutate(&mut req);
            format!(
                "{:#}",
                compose_runtime_snapshot_qualification_binding(req).unwrap_err()
            )
        };
        assert!(
            refused(&|r| r.verifier_artifact_hash = "nope".into()).contains("verifier artifact")
        );
        assert!(refused(&|r| r.provider_spec = Vec::new()).contains("1..="));
        assert!(refused(&|r| r.provider_spec = b"[]".to_vec()).contains("JSON object"));
        assert!(
            refused(&|r| r.consumer_verifiers = Some(serde_json::json!([1])))
                .contains("consumer verifiers")
        );
        assert!(
            refused(&|r| r.consumer_verifiers = Some(serde_json::json!({"s": {"bogus": 1}})))
                .contains("consumer verifiers")
        );
        assert!(refused(&|r| r.limits.maximum_lifetime_seconds = 3601).contains("budget"));
        assert!(refused(&|r| r.snapshot_binding_id = "../x".into()).contains("id is invalid"));
        assert!(refused(&|r| r.id = ".hidden".into()).contains("id is invalid"));
        assert!(
            !node
                .root
                .path()
                .join(".ai/node/runtime_snapshot_qualification")
                .exists()
        );
    }
}
