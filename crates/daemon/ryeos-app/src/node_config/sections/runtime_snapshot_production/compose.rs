//! Composition of one node-signed runtime snapshot production binding.
//!
//! The operator either names an installed external-execution binding, from
//! whose verified document the provider backend, account, adapter artifact
//! identity, Render owner, API trust roots and network inputs are taken so the
//! producer can never drift from the placement binding it feeds, or states
//! backend, account, owner and trust roots outright on a node that places no
//! sessions. The standalone adapter artifact comes from the installed bundles
//! and the network inputs are the operator's, exactly as a placement binding
//! composes them. The operator adds the snapshot production spec, the sandbox
//! group and the source scope. The credential generation is fresh on every
//! composition. This module also holds the signing and publishing steps the
//! qualification composer shares.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ryeos_external_execution_contract::runtime_snapshot::MAX_RUNTIME_SNAPSHOT_UPLOAD_BYTES;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    InstalledRuntimeSnapshotProductionBinding, ProductionDocument,
    RuntimeSnapshotProductionSection, SECTION_NAME,
};
use crate::node_config::NodeConfigSection as _;
use crate::node_config::sections::external_execution::compose::{
    ProducerSourceFacts, canonical_backend_id, item_context, load_node_identity,
    operator_network_inputs, parse_tls_root_pem, publish_signed_document, read_installed_binding,
    read_installed_document, resolve_installed_artifacts, sign_node_document, validate_document_id,
    verify_node_signed,
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

/// Provider facts an operator states for a node that places no sessions and
/// so has no external-execution binding to take them from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneProducerFacts {
    /// Lifecycle backend id; `render` selects render-sandbox-early-access.
    pub backend: String,
    /// Provider account label that scopes the credential.
    pub account: String,
    /// Provider owner the source Sandbox is created under.
    pub owner_id: String,
    /// PEM bundle of the CERTIFICATE roots the provider API is served under.
    pub roots_pem: String,
}

/// Where the producer's provider facts come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProducerFactsSource {
    /// Installed external-execution binding that supplies backend, account,
    /// adapter artifact, owner, trust roots and network inputs.
    ExternalBinding { id: String },
    /// Facts the operator states; the adapter artifact comes from the
    /// installed bundles.
    Standalone(StandaloneProducerFacts),
}

impl ProducerFactsSource {
    /// How a report names the source.
    fn label(&self) -> String {
        match self {
            Self::ExternalBinding { id } => format!("external_binding:{id}"),
            Self::Standalone(_) => "standalone".to_owned(),
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
    pub facts_source: ProducerFactsSource,
    /// Bytes of the snapshot production spec the installed adapter carries.
    pub snapshot_spec: Vec<u8>,
    pub sandbox_group_id: String,
    pub region: String,
    pub source_plan: String,
    /// The generation the binding names for its provider credential; see the
    /// external-execution compose request for who chooses it.
    pub credential_generation: String,
    pub limits: ProductionLimits,
}

/// What was installed. It carries no credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductionComposeReport {
    pub id: String,
    pub path: PathBuf,
    /// `external_binding:<id>` or `standalone`.
    pub source: String,
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
    let facts = match &request.facts_source {
        ProducerFactsSource::ExternalBinding { id } => {
            read_installed_binding(request.app_root, id, &identity)?.producer_source_facts()
        }
        ProducerFactsSource::Standalone(stated) => standalone_source_facts(stated, |backend| {
            Ok(resolve_installed_artifacts(request.app_root)?
                .placement_backends
                .installed_generation(backend)?
                .backend_artifact_hash)
        })?,
    };
    compose_with_facts(request, &identity, facts)
}

/// The facts a placement binding would have supplied, built from stated ones.
/// `adapter_artifact_hash` maps the canonical backend id to the artifact hash
/// of its one installed generation; it runs after the stated values pass
/// their own checks so a malformed request never reads the bundles.
fn standalone_source_facts(
    stated: &StandaloneProducerFacts,
    adapter_artifact_hash: impl FnOnce(&str) -> Result<String>,
) -> Result<ProducerSourceFacts> {
    ensure!(
        identifier(&stated.owner_id, 256),
        "owner id must be 1..=256 characters of A-Z a-z 0-9 _ -"
    );
    ensure!(
        identifier_with_dot(&stated.account, 128),
        "account must be 1..=128 characters of A-Z a-z 0-9 _ - ."
    );
    let roots = parse_tls_root_pem(&stated.roots_pem)?;
    let backend = canonical_backend_id(&stated.backend).to_owned();
    let adapter_artifact_hash = adapter_artifact_hash(&backend)?;
    Ok(ProducerSourceFacts {
        backend,
        account: stated.account.clone(),
        adapter_artifact_hash,
        settings: serde_json::json!({
            "schema": 1,
            "owner_id": stated.owner_id,
            "tls_roots_der_base64": roots,
        }),
        network_inputs: operator_network_inputs(),
    })
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
        credential_generation: request.credential_generation,
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
    let source = request.facts_source.label();
    let path = publish_signed_document(
        request.app_root,
        SECTION_NAME,
        &request.id,
        signed.as_bytes(),
    )?;
    Ok(ProductionComposeReport {
        id: installed.id.clone(),
        path,
        source,
        backend: installed.document.backend.clone(),
        account: installed.document.account.clone(),
        provider_group_id: installed.document.provider_group_id.clone(),
        credential_generation: installed.document.credential_generation.clone(),
        snapshot_spec_sha256: installed.document.snapshot_spec_sha256.clone(),
        settings_digest: installed.document.settings_digest.clone(),
        binding_digest: installed.digest.clone(),
    })
}

/// The credential generation of the production binding installed under `id`,
/// or `None` when no such binding is installed.
pub fn installed_production_credential_generation(
    app_root: &Path,
    id: &str,
) -> Result<Option<String>> {
    validate_document_id(id, "runtime snapshot production binding")?;
    let path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join(SECTION_NAME)
        .join(format!("{id}.yaml"));
    if lillux::read_optional_regular_file_bounded_no_follow(
        &path,
        ryeos_node_runtime::node_document::MAX_ITEM_BYTES,
    )
    .with_context(|| format!("inspect {SECTION_NAME} document {}", path.display()))?
    .is_none()
    {
        return Ok(None);
    }
    let identity = load_node_identity(app_root)?;
    let signed = read_installed_document(app_root, SECTION_NAME, id)?;
    Ok(Some(
        admit_signed_production(app_root, id, &signed, &identity)?
            .document
            .credential_generation,
    ))
}

/// Non-empty run of ASCII alphanumerics, `_`, `-` and `.`, at most `max` long.
fn identifier_with_dot(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
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
    use crate::node_config::sections::external_execution::compose::fresh_credential_generation;

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
            facts_source: ProducerFactsSource::ExternalBinding {
                id: "operator".into(),
            },
            snapshot_spec: SPEC.to_vec(),
            sandbox_group_id: "sbg-exact".into(),
            region: "oregon".into(),
            source_plan: "standard".into(),
            credential_generation: fresh_credential_generation().unwrap(),
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

    fn stated() -> StandaloneProducerFacts {
        StandaloneProducerFacts {
            backend: "render".into(),
            account: "render-test".into(),
            owner_id: "usr-test".into(),
            roots_pem: crate::node_config::sections::external_execution::compose::tests::root_pem(),
        }
    }

    fn standalone_facts() -> ProducerSourceFacts {
        standalone_source_facts(&stated(), |backend| {
            assert_eq!(backend, "render-sandbox-early-access");
            Ok("b".repeat(64))
        })
        .unwrap()
    }

    #[test]
    fn a_standalone_document_equals_the_one_a_binding_with_the_same_facts_yields() {
        let node = stopped_node();
        let identity = load_node_identity(node.root.path()).unwrap();
        let from_stated = standalone_facts();
        // A placement binding's settings carry more than the producer takes;
        // only owner and roots reach the document.
        let mut from_binding = from_stated.clone();
        from_binding.settings = serde_json::json!({
            "schema": 2,
            "owner_id": "usr-test",
            "plan": "pro",
            "region": "frankfurt",
            "snapshot_id": "snp-test",
            "tls_roots_der_base64": from_stated.settings["tls_roots_der_base64"].clone(),
        });
        let generation = fresh_credential_generation().unwrap();
        let compose = |id: &str, facts: ProducerSourceFacts, source: ProducerFactsSource| {
            let mut req = request(&node, id);
            req.credential_generation = generation.clone();
            req.facts_source = source;
            let report = compose_with_facts(req, &identity, facts).unwrap();
            let installed = installed_production_binding(node.root.path(), id, &identity).unwrap();
            (report, installed.document)
        };
        let (binding_report, binding_document) = compose(
            "via-binding",
            from_binding,
            ProducerFactsSource::ExternalBinding {
                id: "operator".into(),
            },
        );
        let (standalone_report, standalone_document) = compose(
            "via-stated",
            from_stated,
            ProducerFactsSource::Standalone(stated()),
        );
        assert_eq!(standalone_document, binding_document);
        assert_eq!(
            standalone_report.settings_digest,
            binding_report.settings_digest
        );
        assert_eq!(binding_report.source, "external_binding:operator");
        assert_eq!(standalone_report.source, "standalone");
        assert_eq!(standalone_document.backend, "render-sandbox-early-access");
        assert_eq!(standalone_document.adapter_artifact_hash, "b".repeat(64));
        assert_eq!(
            standalone_document.network_inputs,
            operator_network_inputs()
        );
    }

    #[test]
    fn stated_facts_are_checked_before_the_bundles_are_read() {
        let refused = |mutate: &dyn Fn(&mut StandaloneProducerFacts)| {
            let mut facts = stated();
            mutate(&mut facts);
            format!(
                "{:#}",
                standalone_source_facts(&facts, |_| panic!("the bundles were read")).unwrap_err()
            )
        };
        assert!(refused(&|f| f.owner_id = "".into()).contains("owner id"));
        assert!(refused(&|f| f.owner_id = "usr test".into()).contains("owner id"));
        assert!(refused(&|f| f.owner_id = "u".repeat(257)).contains("owner id"));
        assert!(refused(&|f| f.account = "".into()).contains("account"));
        assert!(refused(&|f| f.account = "acct name".into()).contains("account"));
        assert!(refused(&|f| f.account = "a".repeat(129)).contains("account"));
        assert!(refused(&|f| f.roots_pem = "".into()).contains("no CERTIFICATE block"));
        assert!(refused(&|f| f.roots_pem = "stray text".into()).contains("only CERTIFICATE"));
        let message = format!(
            "{:#}",
            standalone_source_facts(&stated(), |_| anyhow::bail!("not installed")).unwrap_err()
        );
        assert!(message.contains("not installed"), "{message}");
    }

    #[test]
    fn a_standalone_request_on_a_node_without_bundles_publishes_nothing() {
        let node = stopped_node();
        let mut req = request(&node, "snapshot-producer");
        req.facts_source = ProducerFactsSource::Standalone(stated());
        let error = compose_runtime_snapshot_production_binding(req).unwrap_err();
        assert!(
            format!("{error:#}").contains("load installed bundle registrations"),
            "{error:#}"
        );
        assert!(
            !node
                .root
                .path()
                .join(".ai/node/runtime_snapshot_production")
                .exists()
        );
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
