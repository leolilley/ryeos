//! Composition of one node-signed runtime snapshot production binding.
//!
//! The operator names an installed external-execution binding; the provider
//! backend, account, adapter artifact identity, Render owner, API trust roots
//! and network inputs are taken from that verified document, so the producer
//! can never drift from the placement binding it feeds. The operator adds the
//! snapshot production spec, the sandbox group and the source scope. The
//! credential generation is fresh on every composition. This module also
//! holds the signing and publishing steps the qualification composer shares.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ryeos_external_execution_contract::runtime_snapshot::MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES;
use serde::Serialize;
use serde_json::Value;

use super::{
    InstalledRuntimeSnapshotProductionBinding, ProductionDocument,
    RuntimeSnapshotProductionSection, SECTION_NAME,
};
use crate::node_config::NodeConfigSection as _;
use crate::node_config::sections::external_execution::compose::{
    ProducerSourceFacts, fresh_credential_generation, item_context, load_node_identity,
    publish_signed_document, read_installed_binding, read_installed_document, sign_node_document,
    validate_document_id, verify_node_signed,
};

/// Largest snapshot production spec the adapter accepts.
pub const MAX_SNAPSHOT_SPEC_BYTES: usize = 8 * 1024;

/// Protocol of the production document this composer emits.
const PRODUCTION_PROTOCOL: &str = "ryeos.runtime-snapshot-production.v2";

/// Budgets of one production binding. Omitted fields keep the operator
/// defaults; the document validation owns the hard limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductionLimits {
    pub contact_timeout_seconds: u32,
    pub maximum_bootstrap_lifetime_seconds: u32,
    pub maximum_upload_bytes: u64,
}

impl Default for ProductionLimits {
    fn default() -> Self {
        Self {
            contact_timeout_seconds: 60,
            maximum_bootstrap_lifetime_seconds: 900,
            maximum_upload_bytes: MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES,
        }
    }
}

/// One operator request to compose and install a production binding.
pub struct ProductionComposeRequest<'a> {
    pub app_root: &'a Path,
    /// Proof that the node is stopped; no daemon may read the section while
    /// it is rewritten.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLock,
    pub id: String,
    /// Installed external-execution binding that supplies backend, account,
    /// adapter artifact, owner, trust roots and network inputs.
    pub external_binding_id: String,
    /// Bytes of the snapshot production spec the installed adapter carries.
    pub snapshot_spec: Vec<u8>,
    pub sandbox_group_id: String,
    pub region: String,
    pub source_plan: String,
    pub limits: ProductionLimits,
}

/// What was installed. It carries no credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProductionComposeReport {
    pub id: String,
    pub path: PathBuf,
    pub backend: String,
    pub account: String,
    pub provider_group_id: String,
    pub credential_generation: String,
    pub snapshot_spec_sha256: String,
    pub settings_digest: String,
    pub binding_digest: String,
}

/// Compose, sign with the node identity, verify the exact bytes the loader
/// will read, and publish atomically.
pub fn compose_runtime_snapshot_production_binding(
    request: ProductionComposeRequest<'_>,
) -> Result<ProductionComposeReport> {
    request
        .state_lock
        .ensure_protects_app_root(request.app_root)?;
    validate_document_id(&request.id, "runtime snapshot production binding")?;
    let identity = load_node_identity(request.app_root)?;
    let facts = read_installed_binding(request.app_root, &request.external_binding_id, &identity)?
        .producer_source_facts();
    compose_with_facts(request, &identity, facts)
}

pub(crate) fn compose_with_facts(
    request: ProductionComposeRequest<'_>,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    facts: ProducerSourceFacts,
) -> Result<ProductionComposeReport> {
    ensure!(
        !request.snapshot_spec.is_empty() && request.snapshot_spec.len() <= MAX_SNAPSHOT_SPEC_BYTES,
        "snapshot production spec must be 1..={MAX_SNAPSHOT_SPEC_BYTES} bytes"
    );
    ensure!(
        serde_json::from_slice::<Value>(&request.snapshot_spec)
            .context("snapshot production spec is not JSON")?
            .is_object(),
        "snapshot production spec must be a JSON object"
    );
    ensure!(
        matches!(request.source_plan.as_str(), "starter" | "standard" | "pro"),
        "source plan must be `starter`, `standard` or `pro`"
    );
    ensure!(
        identifier(&request.region, 128),
        "region must be 1..=128 characters of A-Z a-z 0-9 _ -"
    );
    ensure!(
        request
            .sandbox_group_id
            .strip_prefix("sbg-")
            .is_some_and(|rest| identifier(rest, 252)),
        "sandbox group id must be `sbg-` followed by A-Z a-z 0-9 -"
    );
    let owner_id = facts
        .settings
        .get("owner_id")
        .and_then(Value::as_str)
        .context("external-execution settings have no owner_id")?;
    let roots = facts
        .settings
        .get("tls_roots_der_base64")
        .and_then(Value::as_array)
        .filter(|roots| !roots.is_empty())
        .context(
            "external-execution settings carry no tls_roots_der_base64; the producer needs the \
             provider API trust roots",
        )?;
    ensure!(
        roots.len() <= 8,
        "external-execution settings carry more than 8 trust roots"
    );
    let settings = serde_json::json!({
        "schema": 1,
        "owner_id": owner_id,
        "plan": request.source_plan,
        "region": request.region,
        "sandbox_group_id": request.sandbox_group_id,
        "tls_roots_der_base64": roots,
    });
    let document = ProductionDocument {
        kind: "node".into(),
        schema: 1,
        protocol: PRODUCTION_PROTOCOL.into(),
        backend: facts.backend,
        account: facts.account,
        provider_group_id: request.sandbox_group_id,
        credential_generation: fresh_credential_generation()?,
        adapter_artifact_hash: facts.adapter_artifact_hash,
        snapshot_spec_sha256: lillux::sha256_hex(&request.snapshot_spec),
        settings_digest: lillux::sha256_hex(lillux::canonical_json(&settings)?.as_bytes()),
        settings,
        network_inputs: facts.network_inputs,
        contact_timeout_seconds: request.limits.contact_timeout_seconds,
        maximum_bootstrap_lifetime_seconds: request.limits.maximum_bootstrap_lifetime_seconds,
        maximum_upload_bytes: request.limits.maximum_upload_bytes,
    };
    document.validate()?;
    let signed = sign_node_document(&serde_yaml::to_string(&document)?, identity)?;
    let installed = admit_signed_production(request.app_root, &request.id, &signed, identity)?;
    let path = publish_signed_document(
        request.app_root,
        SECTION_NAME,
        &request.id,
        signed.as_bytes(),
    )?;
    Ok(ProductionComposeReport {
        id: installed.id.clone(),
        path,
        backend: installed.document.backend.clone(),
        account: installed.document.account.clone(),
        provider_group_id: installed.document.provider_group_id.clone(),
        credential_generation: installed.document.credential_generation.clone(),
        snapshot_spec_sha256: installed.document.snapshot_spec_sha256.clone(),
        settings_digest: installed.document.settings_digest.clone(),
        binding_digest: installed.digest.clone(),
    })
}

/// Non-empty run of ASCII alphanumerics, `_` and `-`, at most `max` long.
fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

impl InstalledRuntimeSnapshotProductionBinding {
    /// Owner, generation, backend and account of the production credential.
    pub(crate) fn credential_coordinates(&self) -> Result<(String, String, String, String)> {
        let owner = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
            "domain": "ryeos.runtime-snapshot-production-credential.v1",
            "signer": self.signer,
            "backend": self.document.backend,
            "account": self.document.account,
        }))?;
        Ok((
            owner,
            self.document.credential_generation.clone(),
            self.document.backend.clone(),
            self.document.account.clone(),
        ))
    }
}

/// Verify signed production bytes exactly as the loader would and build the
/// installed authority from them.
fn admit_signed_production(
    app_root: &Path,
    id: &str,
    signed: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<InstalledRuntimeSnapshotProductionBinding> {
    let body = verify_node_signed(signed, identity, "runtime snapshot production binding")?;
    let context = item_context(app_root, SECTION_NAME, id, identity.fingerprint());
    // The section parse is the loader's own gate for this document.
    RuntimeSnapshotProductionSection.parse(&context, &body)?;
    let document: ProductionDocument =
        serde_json::from_value(body).context("invalid runtime snapshot production binding")?;
    document.validate()?;
    let signer = identity.fingerprint().to_owned();
    let digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "domain": "ryeos.runtime-snapshot-production-binding.v1",
        "id": id,
        "signer": signer,
        "signed_source": lillux::sha256_hex(signed.as_bytes()),
    }))?;
    Ok(InstalledRuntimeSnapshotProductionBinding {
        id: id.to_owned(),
        document,
        signer,
        signer_verifying_key: identity.verifying_key().to_bytes(),
        signed_source: signed.into(),
        digest,
    })
}

/// Read an installed production binding back the way the loader admits it,
/// for the qualification composer to bind to.
pub(crate) fn installed_production_binding(
    app_root: &Path,
    id: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<InstalledRuntimeSnapshotProductionBinding> {
    validate_document_id(id, "runtime snapshot production binding")?;
    let signed = read_installed_document(app_root, SECTION_NAME, id)?;
    admit_signed_production(app_root, id, &signed, identity)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stopped node: identity plus the state lock.
    pub(crate) struct StoppedNode {
        pub(crate) root: tempfile::TempDir,
        pub(crate) lock: ryeos_node_runtime::state_lock::StateLock,
    }

    pub(crate) fn stopped_node() -> StoppedNode {
        let root = tempfile::tempdir().unwrap();
        ryeos_node_runtime::identity::NodeIdentity::create(
            &root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let lock = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(root.path()),
        )
        .unwrap();
        StoppedNode { root, lock }
    }

    pub(crate) fn facts() -> ProducerSourceFacts {
        ProducerSourceFacts {
            backend: "render-sandbox-early-access".into(),
            account: "render-test".into(),
            adapter_artifact_hash: "b".repeat(64),
            settings: serde_json::json!({
                "schema": 2,
                "owner_id": "usr-test",
                "plan": "standard",
                "region": "oregon",
                "snapshot_id": "snp-test",
                "tls_roots_der_base64": ["fixture"],
            }),
            network_inputs: serde_json::from_value(serde_json::json!({
                "resolver": {"source": "/etc/resolv.conf", "max_bytes": 4096},
                "hosts": {"source": "/etc/hosts", "max_bytes": 4096},
            }))
            .unwrap(),
        }
    }

    pub(crate) const SPEC: &[u8] = br#"{"schema":1,"provider_profile":"render_sandbox_v1"}"#;

    pub(crate) fn request<'a>(node: &'a StoppedNode, id: &str) -> ProductionComposeRequest<'a> {
        ProductionComposeRequest {
            app_root: node.root.path(),
            state_lock: &node.lock,
            id: id.into(),
            external_binding_id: "operator".into(),
            snapshot_spec: SPEC.to_vec(),
            sandbox_group_id: "sbg-exact".into(),
            region: "oregon".into(),
            source_plan: "standard".into(),
            limits: ProductionLimits::default(),
        }
    }

    /// Compose a production binding on the node from fixture facts.
    pub(crate) fn install_production(node: &StoppedNode, id: &str) -> ProductionComposeReport {
        let identity = load_node_identity(node.root.path()).unwrap();
        compose_with_facts(request(node, id), &identity, facts()).unwrap()
    }

    #[test]
    fn installs_a_loader_admissible_document_derived_from_the_facts() {
        let node = stopped_node();
        let report = install_production(&node, "snapshot-producer");
        assert_eq!(report.backend, "render-sandbox-early-access");
        assert_eq!(report.provider_group_id, "sbg-exact");
        assert_eq!(report.snapshot_spec_sha256, lillux::sha256_hex(SPEC));
        assert!(
            report
                .path
                .ends_with(".ai/node/runtime_snapshot_production/snapshot-producer.yaml")
        );
        let identity = load_node_identity(node.root.path()).unwrap();
        let reread =
            installed_production_binding(node.root.path(), "snapshot-producer", &identity).unwrap();
        assert_eq!(reread.digest(), report.binding_digest);
        assert_eq!(reread.settings()["owner_id"], "usr-test");
        assert_eq!(reread.settings()["plan"], "standard");
        assert_eq!(
            reread.settings()["tls_roots_der_base64"],
            serde_json::json!(["fixture"])
        );
        assert_eq!(reread.adapter_artifact_hash(), "b".repeat(64));
    }

    #[test]
    fn two_composes_differ_in_credential_generation() {
        let node = stopped_node();
        let first = install_production(&node, "snapshot-producer");
        let second = install_production(&node, "snapshot-producer");
        assert_ne!(first.credential_generation, second.credential_generation);
        assert_eq!(first.settings_digest, second.settings_digest);
    }

    #[test]
    fn a_lock_that_does_not_protect_the_app_root_is_refused_without_writing() {
        let node = stopped_node();
        let elsewhere = tempfile::tempdir().unwrap();
        let other = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(elsewhere.path()),
        )
        .unwrap();
        let mut req = request(&node, "snapshot-producer");
        req.state_lock = &other;
        assert!(compose_runtime_snapshot_production_binding(req).is_err());
        assert!(
            !node
                .root
                .path()
                .join(".ai/node/runtime_snapshot_production")
                .exists()
        );
    }

    #[test]
    fn invalid_fields_are_refused_and_publish_nothing() {
        let node = stopped_node();
        let identity = load_node_identity(node.root.path()).unwrap();
        let refused = |mutate: &dyn Fn(&mut ProductionComposeRequest<'_>)| {
            let mut req = request(&node, "snapshot-producer");
            mutate(&mut req);
            compose_with_facts(req, &identity, facts())
                .unwrap_err()
                .to_string()
        };
        assert!(refused(&|r| r.snapshot_spec = Vec::new()).contains("1..="));
        assert!(refused(&|r| r.snapshot_spec = b"[]".to_vec()).contains("JSON object"));
        assert!(refused(&|r| r.snapshot_spec = b"nope".to_vec()).contains("not JSON"));
        assert!(refused(&|r| r.sandbox_group_id = "bad group".into()).contains("sandbox group id"));
        assert!(refused(&|r| r.sandbox_group_id = "grp-1".into()).contains("sandbox group id"));
        assert!(refused(&|r| r.sandbox_group_id = "sbg-".into()).contains("sandbox group id"));
        assert!(refused(&|r| r.source_plan = "huge".into()).contains("source plan"));
        assert!(refused(&|r| r.region = "".into()).contains("region"));
        assert!(refused(&|r| r.region = "us west".into()).contains("region"));
        assert!(refused(&|r| r.limits.contact_timeout_seconds = 301).contains("budget"));
        assert!(refused(&|r| r.limits.maximum_upload_bytes = 1).contains("budget"));
        assert!(
            !node
                .root
                .path()
                .join(".ai/node/runtime_snapshot_production")
                .exists()
        );
        assert!(validate_document_id("../x", "binding").is_err());
        assert!(validate_document_id(".hidden", "binding").is_err());
    }

    #[test]
    fn an_absent_external_binding_is_refused() {
        let node = stopped_node();
        let error =
            compose_runtime_snapshot_production_binding(request(&node, "snapshot-producer"))
                .unwrap_err();
        assert!(
            format!("{error:#}").contains("is not installed"),
            "{error:#}"
        );
    }

    #[test]
    fn a_foreign_signed_external_binding_is_refused() {
        let node = stopped_node();
        let foreign = ryeos_node_runtime::identity::NodeIdentity::create(
            &tempfile::tempdir().unwrap().path().join("k.pem"),
        )
        .unwrap();
        let signed = sign_node_document("kind: node\n", &foreign).unwrap();
        publish_signed_document(
            node.root.path(),
            crate::node_config::sections::external_execution::SECTION_NAME,
            "operator",
            signed.as_bytes(),
        )
        .unwrap();
        let error =
            compose_runtime_snapshot_production_binding(request(&node, "snapshot-producer"))
                .unwrap_err();
        assert!(
            format!("{error:#}").contains("signed by the current node"),
            "{error:#}"
        );
    }

    #[test]
    fn producer_trust_roots_must_be_present_and_bounded() {
        let node = stopped_node();
        let identity = load_node_identity(node.root.path()).unwrap();
        let refused = |roots: Option<Value>| {
            let mut source = facts();
            let settings = source.settings.as_object_mut().unwrap();
            match roots {
                Some(roots) => settings.insert("tls_roots_der_base64".into(), roots),
                None => settings.remove("tls_roots_der_base64"),
            };
            format!(
                "{:#}",
                compose_with_facts(request(&node, "snapshot-producer"), &identity, source)
                    .unwrap_err()
            )
        };
        // A binding without provider API roots, such as a file-relay one.
        for message in [refused(None), refused(Some(serde_json::json!([])))] {
            assert!(
                message.contains("carry no tls_roots_der_base64"),
                "{message}"
            );
        }
        let nine = serde_json::json!(["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
        assert!(refused(Some(nine)).contains("more than 8 trust roots"));
        let mut no_owner = facts();
        no_owner
            .settings
            .as_object_mut()
            .unwrap()
            .remove("owner_id");
        let message = format!(
            "{:#}",
            compose_with_facts(request(&node, "snapshot-producer"), &identity, no_owner)
                .unwrap_err()
        );
        assert!(message.contains("no owner_id"), "{message}");
        assert!(
            !node
                .root
                .path()
                .join(".ai/node/runtime_snapshot_production")
                .exists()
        );
    }
}
