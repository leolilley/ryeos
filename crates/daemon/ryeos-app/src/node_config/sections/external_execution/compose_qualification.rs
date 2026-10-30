//! Composition of one node-signed qualification-only structured-session
//! placement binding.
//!
//! A qualification session runs the Worker of a consumer qualification on a
//! fresh external allocation before the runtime it exercises has any
//! qualification of its own. Launch admits it only through exactly one signed
//! binding whose workload is the qualification structured session, so this is
//! the composer of that binding. It follows the ordinary composer: the node is
//! stopped and the state lock held, the document is signed with the node
//! identity, read back through the section's own parse, and published
//! atomically.
//!
//! What the operator states is the provider facts (an installed binding to
//! take them from, or backend, account, owner and API trust roots outright),
//! the Render sandbox settings, the provider declaration id, the signed
//! qualification policy reference, and the identities of the runtime product
//! being qualified. Everything else is read from the node: the requirement
//! digest and Worker profile hash from the signed policy the installed bundles
//! carry, the executable, adapter and connector identities from the verified
//! bundles, and the configuration destination from the installed provider
//! declaration. The runtime manifest names the runtime product the Worker
//! runs; the guest-owner manifest names the measured runtime the provider
//! snapshot was produced from, which the placement compares with the
//! snapshot's record. They coincide only when the qualified runtime is itself
//! the snapshot's owner, so the second is stated separately and defaults to
//! the first.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::compose::{
    BundleArtifactFacts, CLEANUP_PROOF, ComposeLimits, ComposeTransportRequest,
    ProducerSourceFacts, RELAY_NETWORK_POLICY, RENDER_BACKEND_ID, STORAGE_POLICY,
    SelectedTransport, admit_signed_binding, bundle_artifact_facts, load_node_identity,
    publish_signed_document, read_installed_binding, select_transport, sign_node_document,
    validate_document_id, with_installed_bundles,
};
use super::{
    BindingDocument, ExternalQualificationStructuredSessionBinding, ExternalWorkloadBinding,
    InstalledExternalExecutionBinding, SECTION_NAME, adapter_settings_digest,
};
use crate::external_artifacts::ResolvedExternalExecutionArtifacts;
use crate::node_config::sections::runtime_snapshot_production::compose::{
    ProducerFactsSource, identifier, identifier_with_dot, resolve_producer_facts,
};
use ryeos_state::external_execution::admission::{
    CONNECTOR_PROTOCOL, ExternalCandidateQualificationUse, PROTOCOL,
    QUALIFICATION_CONTEXT_PARAMETER,
};

/// Schema of a qualification-only binding over the controller file relay.
const QUALIFICATION_BINDING_SCHEMA: u32 = 18;
/// Schema of the Render sandbox settings the installed adapter accepts.
const RENDER_SETTINGS_SCHEMA: u64 = 2;
/// Most trust roots the Render settings carry.
const MAX_SETTINGS_TLS_ROOTS: usize = 8;
/// Largest signed qualification policy document the composer reads.
const MAX_POLICY_BYTES: u64 = 1024 * 1024;

/// One operator request to compose and install a qualification-only binding.
pub struct QualificationBindingRequest<'a> {
    pub app_root: &'a Path,
    /// Proof that the node is stopped; no daemon may read the section while
    /// it is rewritten.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLock,
    pub id: String,
    /// Where backend, account, Render owner and API trust roots come from.
    pub facts_source: ProducerFactsSource,
    /// Reference of the signed qualification policy the installed bundles
    /// carry, as `config:<category>/<name>`.
    pub policy_ref: String,
    /// Stable id of the installed provider declaration the Worker names.
    pub provider_declaration_id: String,
    /// Manifest hash of the runtime product being qualified; it is also the
    /// guest runtime manifest the placement stages.
    pub runtime_manifest_hash: String,
    /// Manifest hash of the measured guest-owner runtime the provider snapshot
    /// was produced from; the placement refuses a binding whose value differs
    /// from the snapshot operation's. Equal to the runtime manifest only when
    /// the qualified runtime is the snapshot's owner.
    pub guest_runtime_manifest_hash: String,
    /// SHA-256 of the executable member inside that runtime product.
    pub runtime_member_sha256: String,
    /// Provider snapshot the sandbox is restored from.
    pub snapshot_id: String,
    pub plan: String,
    pub region: String,
    /// Absolute private mailbox parent directory of the file relay.
    pub private_mailbox_parent: String,
    pub capacity_group: String,
    /// The generation the binding names for its provider credential; see the
    /// ordinary compose request for who chooses it.
    pub credential_generation: String,
    pub limits: ComposeLimits,
}

/// What was installed. It carries no credential and no vault key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualificationBindingReport {
    pub id: String,
    pub path: PathBuf,
    pub backend: String,
    pub account: String,
    pub capacity_group: String,
    pub provider_declaration_id: String,
    pub requirement_digest: String,
    pub worker_profile_hash: String,
    pub runtime_manifest_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub runtime_member_sha256: String,
    pub snapshot_id: String,
    pub credential_generation: String,
    pub settings_digest: String,
    pub binding_digest: String,
    pub capacity_owner: String,
}

/// What the installed signed qualification policy fixes for the Worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualificationPolicyFacts {
    pub(crate) requirement_digest: String,
    pub(crate) worker_profile_hash: String,
    /// Whether a scenario's Worker request asks for a controller canary,
    /// which the provider must have opted into by declaration.
    pub(crate) controller_canary_requested: bool,
}

/// Compose, sign with the node identity, verify the exact bytes the loader
/// will read, and publish atomically.
pub fn compose_qualification_session_binding(
    request: QualificationBindingRequest<'_>,
) -> Result<QualificationBindingReport> {
    request
        .state_lock
        .ensure_protects_app_root(request.app_root)?;
    check_stated(&request)?;
    let identity = load_node_identity(request.app_root)?;
    let facts = resolve_producer_facts(request.app_root, &identity, &request.facts_source)?;
    let (resolved, policy) =
        with_installed_bundles(request.app_root, |bundle_roots, trust, credential_home| {
            let resolved = crate::external_artifacts::resolve_external_execution_artifacts(
                bundle_roots,
                trust,
                credential_home,
            )?;
            let policy = read_installed_policy_facts(bundle_roots, trust, &request.policy_ref)?;
            Ok((resolved, policy))
        })?;
    compose_with_inputs(request, &identity, facts, &resolved, &policy)
}

/// Every stated value checked against the rule the document or the installed
/// adapter applies, before any bundle is read.
fn check_stated(request: &QualificationBindingRequest<'_>) -> Result<()> {
    validate_document_id(&request.id, "external-execution binding")?;
    ryeos_state::external_content::products::validate_name(&request.provider_declaration_id)
        .context("provider declaration id")?;
    policy_item_path(&request.policy_ref)?;
    for (what, value) in [
        ("runtime manifest hash", &request.runtime_manifest_hash),
        ("guest runtime manifest hash", &request.guest_runtime_manifest_hash),
        ("runtime member sha256", &request.runtime_member_sha256),
        ("credential generation", &request.credential_generation),
    ] {
        ensure!(
            lillux::valid_hash(value),
            "{what} must be a 64-character lowercase hex digest"
        );
    }
    ensure!(
        request.snapshot_id.len() <= 256
            && request
                .snapshot_id
                .strip_prefix("snp-")
                .is_some_and(|rest| !rest.is_empty()
                    && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
        "snapshot id must be `snp-` followed by A-Z a-z 0-9 -, at most 256 characters"
    );
    ensure!(
        matches!(request.plan.as_str(), "starter" | "standard" | "pro"),
        "plan must be `starter`, `standard` or `pro`"
    );
    ensure!(
        identifier(&request.region, 128),
        "region must be 1..=128 characters of A-Z a-z 0-9 _ -"
    );
    ensure!(
        identifier_with_dot(&request.capacity_group, 128),
        "capacity group must be 1..=128 characters of A-Z a-z 0-9 _ - ."
    );
    Ok(())
}

/// The composition itself, parameterised on the provider facts, the resolved
/// artifacts and the policy facts so a test can supply them without a bundle
/// on disk.
pub(crate) fn compose_with_inputs(
    request: QualificationBindingRequest<'_>,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    facts: ProducerSourceFacts,
    resolved: &ResolvedExternalExecutionArtifacts,
    policy: &QualificationPolicyFacts,
) -> Result<QualificationBindingReport> {
    check_stated(&request)?;
    ensure!(
        facts.backend == RENDER_BACKEND_ID,
        "a qualification session is placed over the controller file relay, which only the \
         `{RENDER_BACKEND_ID}` backend provides; got `{}`",
        facts.backend
    );
    let owner_id = facts
        .settings
        .get("owner_id")
        .and_then(Value::as_str)
        .context("provider settings have no owner_id")?;
    ensure!(
        identifier(owner_id, 256),
        "owner id must be 1..=256 characters of A-Z a-z 0-9 _ -"
    );
    let roots = facts
        .settings
        .get("tls_roots_der_base64")
        .and_then(Value::as_array)
        .filter(|roots| !roots.is_empty())
        .context(
            "provider settings carry no tls_roots_der_base64; the Render sandbox settings need \
             the provider API trust roots",
        )?;
    ensure!(
        roots.len() <= MAX_SETTINGS_TLS_ROOTS && roots.iter().all(|root| root.as_str().is_some()),
        "provider settings must carry at most {MAX_SETTINGS_TLS_ROOTS} base64 trust roots"
    );
    let settings = serde_json::json!({
        "schema": RENDER_SETTINGS_SCHEMA,
        "owner_id": owner_id,
        "plan": request.plan,
        "region": request.region,
        "snapshot_id": request.snapshot_id,
        "tls_roots_der_base64": roots,
    });
    let artifacts =
        bundle_artifact_facts(resolved, &request.provider_declaration_id, &facts.backend)?;
    let declaration = resolved
        .provider_configurations
        .declaration_for_provider(&request.provider_declaration_id)?;
    if policy.controller_canary_requested {
        declaration
            .require_qualification_request_binding()
            .context("the qualification policy requests a controller canary")?;
    }
    let mut limits = request.limits.clone();
    // A qualification session exports nothing; the document refuses any other
    // value, so the composer states the one it can sign.
    limits.max_export_bytes = 0;
    crate::external_placement::require_guest_package_artifact_floor_bytes(
        limits.max_guest_package_regular_bytes,
        artifacts.supervisor_artifact_bytes,
        artifacts.launcher_artifact_bytes,
    )?;
    crate::external_placement::require_qualification_channel_limits(limits.max_transfer_bytes)?;
    let transport = select_transport(
        &ComposeTransportRequest::FileRelay {
            owner_workspace_id: owner_id.to_owned(),
            private_mailbox_parent: request.private_mailbox_parent.clone(),
        },
        &settings,
    )?;
    let document = qualification_binding_document(
        &request,
        &facts,
        policy,
        DocumentParts {
            transport,
            settings,
            artifacts,
            provider_configuration_destination: declaration.configuration_destination.clone(),
            limits,
        },
    )?;
    let signed = sign_node_document(&serde_yaml::to_string(&document)?, identity)?;
    let installed = admit_signed_binding(request.app_root, &request.id, &signed, identity)?;
    let path = publish_signed_document(
        request.app_root,
        SECTION_NAME,
        &request.id,
        signed.as_bytes(),
    )?;
    report_of(&installed, path)
}

/// The facts the document takes from the node rather than from the request.
struct DocumentParts {
    transport: SelectedTransport,
    settings: Value,
    artifacts: BundleArtifactFacts,
    provider_configuration_destination: String,
    limits: ComposeLimits,
}

fn qualification_binding_document(
    request: &QualificationBindingRequest<'_>,
    facts: &ProducerSourceFacts,
    policy: &QualificationPolicyFacts,
    parts: DocumentParts,
) -> Result<BindingDocument> {
    let DocumentParts {
        transport,
        settings,
        artifacts,
        provider_configuration_destination,
        limits,
    } = parts;
    let document = BindingDocument {
        kind: "node".into(),
        schema: QUALIFICATION_BINDING_SCHEMA,
        protocol: PROTOCOL.into(),
        workload: ExternalWorkloadBinding::QualificationStructuredSession(
            ExternalQualificationStructuredSessionBinding {
                provider_declaration_id: request.provider_declaration_id.clone(),
                provider_configuration_destination,
                requirement_digest: policy.requirement_digest.clone(),
                worker_profile_hash: policy.worker_profile_hash.clone(),
                runtime_manifest_hash: request.runtime_manifest_hash.clone(),
                runtime_member_sha256: request.runtime_member_sha256.clone(),
                configuration_adapter_artifact_hash: artifacts.configuration_adapter_artifact_hash,
                configuration_adapter_artifact_bytes: artifacts
                    .configuration_adapter_artifact_bytes,
                connector_protocol: CONNECTOR_PROTOCOL.into(),
                connector_artifact_hash: artifacts.connector_artifact_hash,
                connector_artifact_bytes: artifacts.connector_artifact_bytes,
            },
        ),
        backend: facts.backend.clone(),
        account: facts.account.clone(),
        capacity_group: request.capacity_group.clone(),
        credential_generation: request.credential_generation.clone(),
        settings_digest: adapter_settings_digest(&settings)?,
        settings_schema_digest: artifacts.settings_schema_digest,
        settings,
        backend_artifact_hash: artifacts.backend_artifact_hash,
        backend_artifact_bytes: artifacts.backend_artifact_bytes,
        supervisor_artifact_hash: artifacts.supervisor_artifact_hash,
        supervisor_artifact_bytes: artifacts.supervisor_artifact_bytes,
        guest_runtime_manifest_hash: request.guest_runtime_manifest_hash.clone(),
        runtime_qualification: None,
        runtime_content_qualification: None,
        launcher_artifact_hash: artifacts.launcher_artifact_hash,
        launcher_artifact_bytes: artifacts.launcher_artifact_bytes,
        network_policy: RELAY_NETWORK_POLICY.into(),
        storage_policy: STORAGE_POLICY.into(),
        cleanup_proof: CLEANUP_PROOF.into(),
        provider_network_inputs: facts.network_inputs.clone(),
        controller_transport: transport.selection,
        controller_tls_root_certificates_der_base64: transport.tls_roots,
        max_active: limits.max_active,
        timeout_seconds: limits.timeout_seconds,
        contact_timeout_seconds: limits.contact_timeout_seconds,
        observation_timeout_seconds: limits.observation_timeout_seconds,
        cleanup_timeout_seconds: limits.cleanup_timeout_seconds,
        max_workspace_bytes: limits.max_workspace_bytes,
        max_export_bytes: limits.max_export_bytes,
        max_transfer_bytes: limits.max_transfer_bytes,
        max_guest_package_regular_bytes: limits.max_guest_package_regular_bytes,
        max_guest_package_framed_bytes: limits.max_guest_package_framed_bytes,
    };
    document.validate()?;
    Ok(document)
}

fn report_of(
    installed: &InstalledExternalExecutionBinding,
    path: PathBuf,
) -> Result<QualificationBindingReport> {
    let session = installed
        .document
        .workload
        .qualification_structured_session()?;
    Ok(QualificationBindingReport {
        id: installed.id.clone(),
        path,
        backend: installed.document.backend.clone(),
        account: installed.document.account.clone(),
        capacity_group: installed.document.capacity_group.clone(),
        provider_declaration_id: session.provider_declaration_id.clone(),
        requirement_digest: session.requirement_digest.clone(),
        worker_profile_hash: session.worker_profile_hash.clone(),
        runtime_manifest_hash: session.runtime_manifest_hash.clone(),
        guest_runtime_manifest_hash: installed.document.guest_runtime_manifest_hash.clone(),
        runtime_member_sha256: session.runtime_member_sha256.clone(),
        snapshot_id: installed
            .document
            .settings
            .get("snapshot_id")
            .and_then(Value::as_str)
            .context("qualification binding settings have no snapshot_id")?
            .to_owned(),
        credential_generation: installed.document.credential_generation.clone(),
        settings_digest: installed.document.settings_digest.clone(),
        binding_digest: installed.digest.clone(),
        capacity_owner: installed.capacity_owner.clone(),
    })
}

/// The report of the qualification binding already installed under `id`,
/// read back the way the loader admits it.
pub fn installed_qualification_binding_report(
    app_root: &Path,
    id: &str,
) -> Result<QualificationBindingReport> {
    let identity = load_node_identity(app_root)?;
    let installed = read_installed_binding(app_root, id, &identity)?;
    let path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join(SECTION_NAME)
        .join(format!("{id}.yaml"));
    report_of(&installed, path)
}

/// Bundle-relative path of a `config:<category>/<name>` reference.
fn policy_item_path(policy_ref: &str) -> Result<PathBuf> {
    const REFUSAL: &str = "policy reference must be `config:<category>/<name>` of plain \
                           A-Z a-z 0-9 _ - . segments";
    let relative = policy_ref.strip_prefix("config:").context(REFUSAL)?;
    let segments: Vec<&str> = relative.split('/').collect();
    ensure!(segments.len() >= 2, REFUSAL);
    for segment in &segments {
        ensure!(
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
            REFUSAL
        );
    }
    let mut path = PathBuf::from(ryeos_engine::AI_DIR);
    path.push("config");
    if let Some((name, categories)) = segments.split_last() {
        path.extend(categories);
        path.push(format!("{name}.yaml"));
    }
    Ok(path)
}

/// Read the signed qualification policy out of the installed bundle roots and
/// take the Worker identities it fixes. The document must be signed by a
/// signer the node trusts and carry the consumer execution context and the
/// sealed admitted use that launch later compares the Worker against.
pub(crate) fn read_installed_policy_facts(
    bundle_roots: &[PathBuf],
    trust: &ryeos_engine::trust::TrustStore,
    policy_ref: &str,
) -> Result<QualificationPolicyFacts> {
    let relative = policy_item_path(policy_ref)?;
    let mut found = Vec::new();
    for root in bundle_roots {
        let path = root.join(&relative);
        if let Some(bytes) =
            lillux::read_optional_regular_file_bounded_no_follow(&path, MAX_POLICY_BYTES)
                .with_context(|| format!("read qualification policy {}", path.display()))?
        {
            found.push(bytes);
        }
    }
    let bytes = match found.as_slice() {
        [bytes] => bytes,
        [] => bail!("no installed bundle carries qualification policy `{policy_ref}`"),
        _ => bail!("more than one installed bundle carries qualification policy `{policy_ref}`"),
    };
    let signed = std::str::from_utf8(bytes).context("qualification policy is not UTF-8")?;
    let envelope = ryeos_engine::contracts::SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let header = ryeos_engine::item_resolution::parse_signature_header(signed, &envelope)
        .context("qualification policy has no valid signature line")?;
    let (class, _) = ryeos_engine::trust::verify_item_signature(signed, &header, &envelope, trust)
        .context("verify qualification policy signature")?;
    ensure!(
        class == ryeos_engine::contracts::TrustClass::Trusted,
        "qualification policy signature is not trusted"
    );
    let body: Value = serde_yaml::from_str(
        &ryeos_node_runtime::node_config_source::strip_signature(signed),
    )
    .context("parse qualification policy")?;
    ensure!(
        body.get("extends").is_none(),
        "qualification policy composes another document; compose reads only a self-contained policy"
    );
    let policy = body
        .get("product_qualification_policy")
        .context("qualification policy has no product_qualification_policy")?;
    ensure!(
        policy
            .get("consumer_execution_context")
            .is_some_and(Value::is_object),
        "qualification policy has no consumer execution context"
    );
    let parameters = policy
        .get("verifier_parameters")
        .and_then(|parameters| parameters.get(QUALIFICATION_CONTEXT_PARAMETER))
        .with_context(|| {
            format!("qualification policy verifier_parameters carry no {QUALIFICATION_CONTEXT_PARAMETER}")
        })?;
    let admitted_use = ExternalCandidateQualificationUse::from_value(parameters)
        .context("qualification policy admitted use")?;
    let controller_canary_requested = policy
        .get("producer_scenarios")
        .and_then(Value::as_object)
        .is_some_and(|scenarios| {
            scenarios.values().any(|scenario| {
                scenario
                    .get("consumer_worker_request")
                    .and_then(|request| request.get("controller_canary"))
                    .is_some_and(|canary| !canary.is_null())
            })
        });
    Ok(QualificationPolicyFacts {
        requirement_digest: admitted_use.requirement_digest,
        worker_profile_hash: admitted_use.profile_hash,
        controller_canary_requested,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_placement::compose_test_support as installed;
    use crate::node_config::sections::external_execution::compose::tests::root_pem;
    use crate::node_config::sections::runtime_snapshot_production::compose::StandaloneProducerFacts;

    const BINDING_PROTOCOL: &str =
        ryeos_external_execution_contract::qualification_request_binding::QUALIFICATION_REQUEST_BINDING_PROTOCOL;
    const PROVIDER_ID: &str = "codex-hosted";
    const BINDING_ID: &str = "qualification";
    const SETTINGS_SCHEMA: &str =
        "3333333333333333333333333333333333333333333333333333333333333333";

    struct StoppedNode {
        root: tempfile::TempDir,
        lock: ryeos_node_runtime::state_lock::StateLock,
        connector: PathBuf,
        adapter: PathBuf,
    }

    fn stopped_node() -> StoppedNode {
        let root = tempfile::tempdir().unwrap();
        ryeos_node_runtime::identity::NodeIdentity::create(
            &root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let lock = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(root.path()),
        )
        .unwrap();
        let connector = root.path().join("connector");
        installed::write_executable(&connector, b"connector executable bytes");
        let adapter = root.path().join("configuration-adapter");
        installed::write_executable(&adapter, b"configuration adapter executable bytes");
        StoppedNode {
            root,
            lock,
            connector,
            adapter,
        }
    }

    fn identity(node: &StoppedNode) -> ryeos_node_runtime::identity::NodeIdentity {
        load_node_identity(node.root.path()).unwrap()
    }

    fn declaration(
        id: &str,
        binding_protocol: Option<&str>,
    ) -> ryeos_external_execution_contract::ExternalProviderDeclaration {
        ryeos_external_execution_contract::ExternalProviderDeclaration {
            id: id.to_owned(),
            protocol: ryeos_external_execution_contract::PROVIDER_DECLARATION_PROTOCOL.to_owned(),
            targets: vec!["x86_64-unknown-linux-gnu".to_owned()],
            connector: "provider-connector".to_owned(),
            connector_process_group:
                ryeos_external_execution_contract::ExternalProviderConnectorProcessGroup::New,
            configuration_adapter: "provider-configuration".to_owned(),
            configuration_destination: "environments.toml".to_owned(),
            qualification_request_binding_protocol: binding_protocol.map(str::to_owned),
        }
    }

    fn resolved(
        node: &StoppedNode,
        binding_protocol: Option<&str>,
        backend: bool,
    ) -> ResolvedExternalExecutionArtifacts {
        let (connector_hash, _) = installed::connector_coordinate(&node.connector);
        let backends = if backend {
            vec![installed::CoordinateOnlyBackend::shared(
                RENDER_BACKEND_ID,
                &"d".repeat(64),
                SETTINGS_SCHEMA,
                &"1".repeat(64),
                &"e".repeat(64),
            )]
        } else {
            Vec::new()
        };
        ResolvedExternalExecutionArtifacts {
            provider_connector_hashes: std::collections::BTreeMap::from([(
                PROVIDER_ID.to_owned(),
                connector_hash,
            )]),
            connectors: installed::connector_registry([node.connector.clone()]).unwrap(),
            provider_configurations:
                crate::external_placement::ExternalProviderConfigurationRegistry::from_artifacts(
                    vec![
                        installed::provider_configuration(
                            declaration(PROVIDER_ID, binding_protocol),
                            &node.adapter,
                        )
                        .unwrap(),
                    ],
                )
                .unwrap(),
            placement_backends:
                crate::external_placement::ExternalPlacementBackendRegistry::from_backends(backends)
                    .unwrap(),
        }
    }

    fn facts() -> ProducerSourceFacts {
        ProducerSourceFacts {
            backend: RENDER_BACKEND_ID.into(),
            account: "render-test".into(),
            adapter_artifact_hash: "d".repeat(64),
            settings: serde_json::json!({
                "schema": 2,
                "owner_id": "usr-test",
                "tls_roots_der_base64": ["Zml4dHVyZQ=="],
            }),
            network_inputs: super::super::compose::operator_network_inputs(),
        }
    }

    fn policy() -> QualificationPolicyFacts {
        QualificationPolicyFacts {
            requirement_digest: "a".repeat(64),
            worker_profile_hash: "b".repeat(64),
            controller_canary_requested: true,
        }
    }

    fn request<'a>(node: &'a StoppedNode, id: &str) -> QualificationBindingRequest<'a> {
        QualificationBindingRequest {
            app_root: node.root.path(),
            state_lock: &node.lock,
            id: id.into(),
            facts_source: ProducerFactsSource::ExternalBinding {
                id: "operator".into(),
            },
            policy_ref: "config:codex/consumer-qualification-policy".into(),
            provider_declaration_id: PROVIDER_ID.into(),
            runtime_manifest_hash: "c".repeat(64),
            guest_runtime_manifest_hash: "9".repeat(64),
            runtime_member_sha256: "e".repeat(64),
            snapshot_id: "snp-exact".into(),
            plan: "standard".into(),
            region: "oregon".into(),
            private_mailbox_parent: "/run/ryeos/private-relay".into(),
            capacity_group: "qualification-workers".into(),
            credential_generation: super::super::compose::fresh_credential_generation().unwrap(),
            limits: ComposeLimits::default(),
        }
    }

    fn compose(
        node: &StoppedNode,
        mutate: impl FnOnce(&mut QualificationBindingRequest<'_>),
    ) -> Result<QualificationBindingReport> {
        let mut req = request(node, BINDING_ID);
        mutate(&mut req);
        compose_with_inputs(
            req,
            &identity(node),
            facts(),
            &resolved(node, Some(BINDING_PROTOCOL), true),
            &policy(),
        )
    }

    fn published(node: &StoppedNode) -> bool {
        node.root
            .path()
            .join(".ai/node/external_execution")
            .join(format!("{BINDING_ID}.yaml"))
            .exists()
    }

    #[test]
    fn a_composed_binding_is_loader_admissible_and_derived_from_the_inputs() {
        let node = stopped_node();
        let report = compose(&node, |_| {}).unwrap();
        assert_eq!(report.id, BINDING_ID);
        assert_eq!(report.backend, RENDER_BACKEND_ID);
        assert_eq!(report.requirement_digest, "a".repeat(64));
        assert_eq!(report.worker_profile_hash, "b".repeat(64));
        assert_eq!(report.snapshot_id, "snp-exact");
        assert!(
            report
                .path
                .ends_with(".ai/node/external_execution/qualification.yaml")
        );
        let identity = identity(&node);
        let signed = std::fs::read_to_string(&report.path).unwrap();
        let loaded =
            admit_signed_binding(node.root.path(), BINDING_ID, &signed, &identity).unwrap();
        loaded.retained_generation().unwrap();
        assert_eq!(loaded.digest, report.binding_digest);
        assert_eq!(loaded.capacity_owner, report.capacity_owner);
        let document = &loaded.document;
        assert_eq!(document.schema, 18);
        assert_eq!(document.max_export_bytes, 0);
        assert!(document.runtime_qualification.is_none());
        assert!(document.runtime_content_qualification.is_none());
        assert_eq!(document.guest_runtime_manifest_hash, "9".repeat(64));
        assert_eq!(report.guest_runtime_manifest_hash, "9".repeat(64));
        assert_eq!(document.network_policy, RELAY_NETWORK_POLICY);
        assert_eq!(document.settings_schema_digest, SETTINGS_SCHEMA);
        assert_eq!(document.settings["owner_id"], "usr-test");
        assert_eq!(document.settings["plan"], "standard");
        assert_eq!(document.settings["region"], "oregon");
        assert_eq!(document.settings["snapshot_id"], "snp-exact");
        assert_eq!(document.settings["schema"], 2);
        let session = document
            .workload
            .qualification_structured_session()
            .unwrap();
        assert_eq!(session.provider_declaration_id, PROVIDER_ID);
        assert_eq!(
            session.provider_configuration_destination,
            "environments.toml"
        );
        assert_eq!(session.runtime_manifest_hash, "c".repeat(64));
        assert_eq!(session.runtime_member_sha256, "e".repeat(64));
        assert_eq!(session.connector_protocol, CONNECTOR_PROTOCOL);
        let (connector_hash, connector_bytes) = installed::connector_coordinate(&node.connector);
        assert_eq!(session.connector_artifact_hash, connector_hash);
        assert_eq!(session.connector_artifact_bytes, connector_bytes);
        let (adapter_hash, adapter_bytes) =
            installed::provider_configuration_coordinate(&node.adapter);
        assert_eq!(session.configuration_adapter_artifact_hash, adapter_hash);
        assert_eq!(session.configuration_adapter_artifact_bytes, adapter_bytes);
        assert!(document.workload.structured_session().is_err());
        // The installed document reports the same facts the compose did.
        assert_eq!(
            installed_qualification_binding_report(node.root.path(), BINDING_ID).unwrap(),
            report
        );
    }

    #[test]
    fn a_requested_export_budget_is_signed_as_zero() {
        let node = stopped_node();
        let report = compose(&node, |r| r.limits.max_export_bytes = 4096).unwrap();
        let identity = identity(&node);
        let signed = std::fs::read_to_string(&report.path).unwrap();
        let loaded =
            admit_signed_binding(node.root.path(), BINDING_ID, &signed, &identity).unwrap();
        assert_eq!(loaded.document.max_export_bytes, 0);
    }

    #[test]
    fn two_composes_differ_in_credential_generation_only() {
        let node = stopped_node();
        let first = compose(&node, |_| {}).unwrap();
        let second = compose(&node, |_| {}).unwrap();
        assert_ne!(first.credential_generation, second.credential_generation);
        assert_eq!(first.settings_digest, second.settings_digest);
    }

    #[test]
    fn stated_values_are_refused_and_publish_nothing() {
        let node = stopped_node();
        let refused = |mutate: &dyn Fn(&mut QualificationBindingRequest<'_>)| {
            format!("{:#}", compose(&node, |r| mutate(r)).unwrap_err())
        };
        assert!(refused(&|r| r.id = "../x".into()).contains("id is invalid"));
        assert!(refused(&|r| r.provider_declaration_id = "bad id".into()).contains("provider"));
        assert!(refused(&|r| r.policy_ref = "tool:codex/x".into()).contains("policy reference"));
        assert!(refused(&|r| r.policy_ref = "config:../x".into()).contains("policy reference"));
        assert!(refused(&|r| r.policy_ref = "config:policy".into()).contains("policy reference"));
        assert!(
            refused(&|r| r.runtime_manifest_hash = "X".repeat(64))
                .contains("runtime manifest hash must be"),
        );
        assert!(
            refused(&|r| r.guest_runtime_manifest_hash = "X".repeat(64))
                .contains("runtime manifest hash")
        );
        assert!(
            refused(&|r| r.runtime_member_sha256 = "e".repeat(63))
                .contains("runtime member sha256")
        );
        assert!(refused(&|r| r.credential_generation = "g".into()).contains("credential"));
        let too_long = format!("snp-{}", "a".repeat(253));
        for bad in [
            "",
            "snp-",
            "snapshot",
            "snp-a b",
            "snp-a_b",
            too_long.as_str(),
        ] {
            assert!(
                refused(&|r| r.snapshot_id = bad.into()).contains("snapshot id"),
                "{bad}"
            );
        }
        assert!(refused(&|r| r.plan = "huge".into()).contains("plan"));
        assert!(refused(&|r| r.region = "us west".into()).contains("region"));
        assert!(refused(&|r| r.capacity_group = "".into()).contains("capacity group"));
        assert!(
            refused(&|r| r.private_mailbox_parent = "relative/path".into())
                .contains("mailbox parent")
        );
        assert!(
            refused(&|r| r.private_mailbox_parent = "/run/../x".into()).contains("mailbox parent")
        );
        assert!(refused(&|r| r.limits.max_active = 0).contains("limits"));
        assert!(!published(&node));
    }

    #[test]
    fn provider_facts_the_settings_cannot_take_are_refused() {
        let node = stopped_node();
        let compose_facts = |facts: ProducerSourceFacts| {
            format!(
                "{:#}",
                compose_with_inputs(
                    request(&node, BINDING_ID),
                    &identity(&node),
                    facts,
                    &resolved(&node, Some(BINDING_PROTOCOL), true),
                    &policy(),
                )
                .unwrap_err()
            )
        };
        let mut other_backend = facts();
        other_backend.backend = "another-backend".into();
        assert!(compose_facts(other_backend).contains("controller file relay"));
        let mut no_owner = facts();
        no_owner
            .settings
            .as_object_mut()
            .unwrap()
            .remove("owner_id");
        assert!(compose_facts(no_owner).contains("no owner_id"));
        let mut bad_owner = facts();
        bad_owner.settings["owner_id"] = "usr test".into();
        assert!(compose_facts(bad_owner).contains("owner id"));
        for roots in [
            None,
            Some(serde_json::json!([])),
            Some(serde_json::json!([1])),
        ] {
            let mut source = facts();
            let settings = source.settings.as_object_mut().unwrap();
            match roots {
                Some(roots) => settings.insert("tls_roots_der_base64".into(), roots),
                None => settings.remove("tls_roots_der_base64"),
            };
            let message = compose_facts(source);
            assert!(
                message.contains("tls_roots_der_base64") || message.contains("trust roots"),
                "{message}"
            );
        }
        let nine = serde_json::json!(["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
        let mut many = facts();
        many.settings["tls_roots_der_base64"] = nine;
        assert!(compose_facts(many).contains("at most 8"));
        assert!(!published(&node));
    }

    #[test]
    fn installed_artifact_mismatches_are_refused() {
        let node = stopped_node();
        let refused = |resolved: ResolvedExternalExecutionArtifacts,
                       mutate: &dyn Fn(&mut QualificationBindingRequest<'_>),
                       policy: QualificationPolicyFacts| {
            let mut req = request(&node, BINDING_ID);
            mutate(&mut req);
            format!(
                "{:#}",
                compose_with_inputs(req, &identity(&node), facts(), &resolved, &policy)
                    .unwrap_err()
            )
        };
        assert!(
            refused(
                resolved(&node, Some(BINDING_PROTOCOL), true),
                &|r| r.provider_declaration_id = "another-provider".into(),
                policy()
            )
            .contains("external provider declaration is not installed")
        );
        assert!(
            refused(
                resolved(&node, Some(BINDING_PROTOCOL), false),
                &|_| {},
                policy()
            )
            .contains("`render-sandbox-early-access` is not installed")
        );
        // A canary-requesting policy needs the provider's opt-in; one that
        // asks for no canary does not.
        assert!(
            refused(resolved(&node, None, true), &|_| {}, policy())
                .contains("does not admit qualification request binding")
        );
        // 8192 + 12288 bytes of guest executables exceed this regular budget.
        assert!(
            refused(
                resolved(&node, Some(BINDING_PROTOCOL), true),
                &|r| {
                    r.limits.max_guest_package_regular_bytes = 20_000;
                    r.limits.max_guest_package_framed_bytes = 40_000;
                },
                policy()
            )
            .contains("exceed the signed regular package budget")
        );
        assert!(!published(&node));
        let mut no_canary = policy();
        no_canary.controller_canary_requested = false;
        compose_with_inputs(
            request(&node, BINDING_ID),
            &identity(&node),
            facts(),
            &resolved(&node, None, true),
            &no_canary,
        )
        .unwrap();
        assert!(published(&node));
    }

    #[test]
    fn a_lock_that_does_not_protect_the_app_root_is_refused_without_writing() {
        let node = stopped_node();
        let elsewhere = tempfile::tempdir().unwrap();
        let other = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(elsewhere.path()),
        )
        .unwrap();
        let mut req = request(&node, BINDING_ID);
        req.state_lock = &other;
        assert!(compose_qualification_session_binding(req).is_err());
        assert!(!published(&node));
    }

    #[test]
    fn stated_values_are_checked_before_the_bundles_are_read() {
        let node = stopped_node();
        let mut req = request(&node, BINDING_ID);
        req.plan = "huge".into();
        let error = compose_qualification_session_binding(req).unwrap_err();
        assert!(format!("{error:#}").contains("plan"), "{error:#}");
        // A well-formed request on a node with no bundles stops at the
        // provider facts, which here are stated and need the adapter.
        let mut req = request(&node, BINDING_ID);
        req.facts_source = ProducerFactsSource::Standalone(StandaloneProducerFacts {
            backend: "render".into(),
            account: "render-test".into(),
            owner_id: "usr-test".into(),
            roots_pem: root_pem(),
        });
        let error = compose_qualification_session_binding(req).unwrap_err();
        assert!(
            format!("{error:#}").contains("load installed bundle registrations"),
            "{error:#}"
        );
        assert!(!published(&node));
    }

    #[test]
    fn a_composed_binding_names_the_credential_provisioning_coordinates() {
        let node = stopped_node();
        let report = compose(&node, |_| {}).unwrap();
        let identity = identity(&node);
        let loaded = read_installed_binding(node.root.path(), BINDING_ID, &identity).unwrap();
        let (owner, generation, backend, account) = loaded.credential_coordinates();
        assert_eq!(owner, report.capacity_owner);
        assert_eq!(generation, report.credential_generation);
        assert_eq!(backend, RENDER_BACKEND_ID);
        assert_eq!(account, "render-test");
        assert_eq!(
            super::super::compose::installed_binding_credential_generation(
                node.root.path(),
                BINDING_ID
            )
            .unwrap(),
            Some(report.credential_generation)
        );
    }

    #[test]
    fn policy_references_are_plain_config_paths() {
        assert_eq!(
            policy_item_path("config:codex/consumer-qualification-policy").unwrap(),
            PathBuf::from(".ai/config/codex/consumer-qualification-policy.yaml")
        );
        for bad in [
            "",
            "config:",
            "config:one",
            "config:a//b",
            "config:a/.b",
            "config:a/../b",
            "config:/a/b",
            "config:a/b c",
            "tool:a/b",
        ] {
            assert!(policy_item_path(bad).is_err(), "{bad}");
        }
    }

    // ── Policy reading ───────────────────────────────────────────────────

    struct PolicyHome {
        _dir: tempfile::TempDir,
        roots: Vec<PathBuf>,
        trust: ryeos_engine::trust::TrustStore,
    }

    fn policy_document(canary: bool) -> String {
        let context = serde_json::json!({
            "schema": "ryeos.external_candidate_qualification_context.v3",
            "requirement_digest": "1".repeat(64),
            "profile_hash": "2".repeat(64),
            "source_binding_hash": "3".repeat(64),
            "source_content_manifest_hash": "4".repeat(64),
            "provider_executable_manifest_hash": "5".repeat(64),
            "execution_environment_digest": "6".repeat(64),
        });
        let request = if canary {
            serde_json::json!({"controller_canary": "original_controller_read_and_local_refusal_v1"})
        } else {
            serde_json::json!({"controller_canary": null})
        };
        serde_yaml::to_string(&serde_json::json!({
            "category": "codex",
            "product_qualification_policy": {
                "verifier_parameters": {
                    (QUALIFICATION_CONTEXT_PARAMETER): context,
                },
                "consumer_execution_context": {"worker_ref": "worker:codex/w"},
                "producer_scenarios": {
                    "remote_codex": {"consumer_worker_request": request},
                },
            },
        }))
        .unwrap()
    }

    fn policy_home(documents: &[(&str, &str)], sign: bool) -> PolicyHome {
        let dir = tempfile::tempdir().unwrap();
        let signer = ryeos_node_runtime::identity::NodeIdentity::create(
            &dir.path().join("signer/private_key.pem"),
        )
        .unwrap();
        let mut roots = Vec::new();
        for (name, body) in documents {
            let root = dir.path().join(name);
            let file = root.join(".ai/config/codex/consumer-qualification-policy.yaml");
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let text = if sign {
                sign_node_document(body, &signer).unwrap()
            } else {
                (*body).to_owned()
            };
            std::fs::write(&file, text).unwrap();
            roots.push(root);
        }
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: signer.fingerprint().to_owned(),
                verifying_key: *signer.verifying_key(),
                label: None,
            },
        ]);
        PolicyHome {
            _dir: dir,
            roots,
            trust,
        }
    }

    const POLICY_REF: &str = "config:codex/consumer-qualification-policy";

    #[test]
    fn the_worker_identities_come_from_the_signed_policy() {
        let document = policy_document(true);
        let home = policy_home(&[("bundle", document.as_str())], true);
        let facts = read_installed_policy_facts(&home.roots, &home.trust, POLICY_REF).unwrap();
        assert_eq!(facts.requirement_digest, "1".repeat(64));
        assert_eq!(facts.worker_profile_hash, "2".repeat(64));
        assert!(facts.controller_canary_requested);
        let document = policy_document(false);
        let home = policy_home(&[("bundle", document.as_str())], true);
        assert!(
            !read_installed_policy_facts(&home.roots, &home.trust, POLICY_REF)
                .unwrap()
                .controller_canary_requested
        );
    }

    #[test]
    fn a_policy_that_is_unsigned_untrusted_absent_or_ambiguous_is_refused() {
        let document = policy_document(true);
        let unsigned = policy_home(&[("bundle", document.as_str())], false);
        let error =
            read_installed_policy_facts(&unsigned.roots, &unsigned.trust, POLICY_REF).unwrap_err();
        assert!(format!("{error:#}").contains("signature"), "{error:#}");

        let home = policy_home(&[("bundle", document.as_str())], true);
        let stranger = ryeos_engine::trust::TrustStore::from_signers(Vec::new());
        let error = read_installed_policy_facts(&home.roots, &stranger, POLICY_REF).unwrap_err();
        assert!(format!("{error:#}").contains("signature"), "{error:#}");

        let error = read_installed_policy_facts(&home.roots, &home.trust, "config:codex/other")
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("no installed bundle carries"),
            "{error:#}"
        );
        let error = read_installed_policy_facts(&[], &home.trust, POLICY_REF).unwrap_err();
        assert!(
            format!("{error:#}").contains("no installed bundle carries"),
            "{error:#}"
        );

        let twice = policy_home(
            &[("one", document.as_str()), ("two", document.as_str())],
            true,
        );
        let error =
            read_installed_policy_facts(&twice.roots, &twice.trust, POLICY_REF).unwrap_err();
        assert!(
            format!("{error:#}").contains("more than one installed bundle"),
            "{error:#}"
        );
    }

    #[test]
    fn a_policy_without_the_sealed_context_is_refused() {
        let refused = |body: String| {
            let home = policy_home(&[("bundle", body.as_str())], true);
            format!(
                "{:#}",
                read_installed_policy_facts(&home.roots, &home.trust, POLICY_REF).unwrap_err()
            )
        };
        let base: Value = serde_yaml::from_str(&policy_document(true)).unwrap();
        let yaml = |value: &Value| serde_yaml::to_string(value).unwrap();

        let mut extends = base.clone();
        extends["extends"] = "config:codex/base".into();
        assert!(refused(yaml(&extends)).contains("composes another document"));

        let mut no_policy = base.clone();
        no_policy
            .as_object_mut()
            .unwrap()
            .remove("product_qualification_policy");
        assert!(refused(yaml(&no_policy)).contains("no product_qualification_policy"));

        let mut no_context = base.clone();
        no_context["product_qualification_policy"]
            .as_object_mut()
            .unwrap()
            .remove("consumer_execution_context");
        assert!(refused(yaml(&no_context)).contains("no consumer execution context"));

        let mut no_parameters = base.clone();
        no_parameters["product_qualification_policy"]["verifier_parameters"] =
            serde_json::json!({});
        assert!(refused(yaml(&no_parameters)).contains(QUALIFICATION_CONTEXT_PARAMETER));

        let mut bad_digest = base;
        bad_digest["product_qualification_policy"]["verifier_parameters"]
            [QUALIFICATION_CONTEXT_PARAMETER]["profile_hash"] = "not-a-digest".into();
        assert!(refused(yaml(&bad_digest)).contains("admitted use"));
    }
}
