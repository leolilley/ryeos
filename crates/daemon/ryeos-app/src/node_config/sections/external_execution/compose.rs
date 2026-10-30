//! Composition of one node-signed external placement binding.
//!
//! `compose_binding_document` is the only constructor of a binding document
//! outside the loader. Test fixtures and the offline operator command both
//! call it, with the same runtime qualification inputs, so a fixture can never
//! drift from what an operator composes. The operator path adds the facts only an installed node can supply (artifact
//! identities from the verified bundles), a fresh credential generation, the
//! node-identity signature and an exact round trip through the section parse
//! before anything is published.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_state::external_execution::admission::{
    AdmittedExternalCandidateProgram, ExternalCandidateRuntimeSource,
};
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_RELAY_CONTRACT, EXTERNAL_CHANNEL_ROUTE_CONTRACT,
    ExternalControllerFileRelayContract, ExternalControllerTransportContract,
    ExternalControllerTransportSelection, ExternalNetworkInputPolicy,
    ExternalNetworkInputSelection, MAX_TLS_ROOT_CERTIFICATES, external_tls_root_bundle_digest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    BindingDocument, ExternalExecutionSection, ExternalRuntimeContentQualificationBinding,
    ExternalRuntimeQualificationBinding, ExternalStructuredSessionBinding, ExternalWorkloadBinding,
    InstalledExternalExecutionBinding, SECTION_NAME, adapter_settings_digest, binding_digest,
    capacity_owner,
};
use crate::node_config::NodeConfigSection as _;

/// Backend id the `render` shorthand selects.
pub const RENDER_BACKEND_ID: &str = "render-sandbox-early-access";
/// Capacity group an operator binding joins unless told otherwise.
pub const DEFAULT_CAPACITY_GROUP: &str = "candidate-workers";

const HTTP_NETWORK_POLICY: &str = "supervisor_pinned_owner_only_candidate_denied_v1";
pub(super) const RELAY_NETWORK_POLICY: &str = "controller_file_relay_guest_deny_all_v1";
pub(super) const STORAGE_POLICY: &str = "ephemeral_private_candidate_v1";
pub(super) const CLEANUP_PROOF: &str = "provider_terminal_occurrence_v1";
const MAX_ROOTS_PEM_BYTES: usize = 256 * 1024;

/// Resolve the operator's backend spelling to the installed adapter id.
pub fn canonical_backend_id(backend: &str) -> &str {
    if backend == "render" {
        RENDER_BACKEND_ID
    } else {
        backend
    }
}

/// Bounds a binding grants one placement. Every omitted field keeps the
/// operator default; the document validation still owns the hard limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComposeLimits {
    pub max_active: u16,
    pub timeout_seconds: u32,
    pub contact_timeout_seconds: u32,
    pub observation_timeout_seconds: u32,
    pub cleanup_timeout_seconds: u32,
    pub max_workspace_bytes: u64,
    pub max_export_bytes: u64,
    pub max_transfer_bytes: u64,
    pub max_guest_package_regular_bytes: u64,
    pub max_guest_package_framed_bytes: u64,
}

impl Default for ComposeLimits {
    fn default() -> Self {
        Self {
            max_active: 1,
            timeout_seconds: 600,
            contact_timeout_seconds: 30,
            observation_timeout_seconds: 120,
            cleanup_timeout_seconds: 300,
            max_workspace_bytes: 1024 * 1024 * 1024,
            max_export_bytes: 512 * 1024 * 1024,
            max_transfer_bytes: 1024 * 1024 * 1024,
            max_guest_package_regular_bytes: 1024 * 1024 * 1024,
            max_guest_package_framed_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// Artifact identities taken from the installed, verified bundles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BundleArtifactFacts {
    pub(crate) backend_artifact_hash: String,
    pub(crate) backend_artifact_bytes: u64,
    pub(crate) settings_schema_digest: String,
    pub(crate) supervisor_artifact_hash: String,
    pub(crate) supervisor_artifact_bytes: u64,
    pub(crate) launcher_artifact_hash: String,
    pub(crate) launcher_artifact_bytes: u64,
    pub(crate) configuration_adapter_artifact_hash: String,
    pub(crate) configuration_adapter_artifact_bytes: u64,
    pub(crate) connector_artifact_hash: String,
    pub(crate) connector_artifact_bytes: u64,
}

/// Everything one binding document is made from.
pub(crate) struct ComposeInputs<'a> {
    pub(crate) program: &'a AdmittedExternalCandidateProgram,
    pub(crate) controller_transport: ExternalControllerTransportSelection,
    pub(crate) provider_network_inputs: ExternalNetworkInputPolicy,
    pub(crate) tls_roots_der_base64: Vec<String>,
    pub(crate) backend: String,
    pub(crate) account: String,
    pub(crate) capacity_group: String,
    pub(crate) credential_generation: String,
    pub(crate) settings: Value,
    pub(crate) guest_runtime_manifest_hash: String,
    pub(crate) runtime_qualification: Option<ExternalRuntimeQualificationBinding>,
    pub(crate) runtime_content_qualification: Option<ExternalRuntimeContentQualificationBinding>,
    pub(crate) artifacts: BundleArtifactFacts,
    pub(crate) limits: ComposeLimits,
}

/// A runtime content qualification is the structured session's testimony
/// that its runtime content is compatible; launch loads it by activation,
/// coordinate and attestation and checks it against the session's runtime
/// manifest, whichever lane the program's runtime came from. A program
/// derived from activated content was itself derived from that testimony, so
/// it must be present and must name the exact coordinate and attestation the
/// program carries; otherwise a binding that launch would reject on its
/// selection identity would be signed.
fn require_content_qualification_pairing(
    program: &AdmittedExternalCandidateProgram,
    qualification: Option<&ExternalRuntimeContentQualificationBinding>,
) -> Result<()> {
    let ExternalCandidateRuntimeSource::ActivatedContent {
        qualification_coordinate_id,
        ..
    } = &program.runtime_source
    else {
        return Ok(());
    };
    let qualification = qualification
        .context("an activated-content program requires a runtime content qualification")?;
    ensure!(
        qualification.coordinate_id == *qualification_coordinate_id,
        "runtime content qualification names a coordinate the program was not derived from"
    );
    ensure!(
        qualification.attestation_hash == program.qualification_attestation_hash,
        "runtime content qualification names an attestation the program was not derived from"
    );
    Ok(())
}

/// The single non-test constructor of a placement binding document. The
/// schema, network policy, storage policy and cleanup proof follow from the
/// transport; the digest of the settings is derived, never supplied.
pub(super) fn compose_binding_document(inputs: ComposeInputs<'_>) -> Result<BindingDocument> {
    let ComposeInputs {
        program,
        controller_transport,
        provider_network_inputs,
        tls_roots_der_base64,
        backend,
        account,
        capacity_group,
        credential_generation,
        settings,
        guest_runtime_manifest_hash,
        runtime_qualification,
        runtime_content_qualification,
        artifacts,
        limits,
    } = inputs;
    program.validate()?;
    require_content_qualification_pairing(program, runtime_content_qualification.as_ref())?;
    controller_transport.validate()?;
    provider_network_inputs.validate()?;
    let (schema, network_policy) = match &controller_transport {
        ExternalControllerTransportSelection::HttpJson(http) => {
            ensure!(
                external_tls_root_bundle_digest(&tls_roots_der_base64)?
                    == http.tls_root_bundle_digest,
                "composed TLS roots changed their transport identity"
            );
            (17, HTTP_NETWORK_POLICY)
        }
        ExternalControllerTransportSelection::ControllerFileRelay(_) => {
            ensure!(
                tls_roots_der_base64.is_empty(),
                "a file-relay transport carries no TLS roots"
            );
            (19, RELAY_NETWORK_POLICY)
        }
    };
    let document = BindingDocument {
        kind: "node".into(),
        schema,
        protocol: program.requirement.protocol.clone(),
        workload: ExternalWorkloadBinding::StructuredSession(ExternalStructuredSessionBinding {
            provider_declaration_id: program.requirement.provider_declaration_id.clone(),
            provider_configuration_destination: program
                .requirement
                .provider_configuration_destination
                .clone(),
            runtime_manifest_hash: program.runtime_manifest_hash.clone(),
            runtime_selection_identity: program.selection_identity_digest.clone(),
            configuration_adapter_artifact_hash: artifacts.configuration_adapter_artifact_hash,
            configuration_adapter_artifact_bytes: artifacts.configuration_adapter_artifact_bytes,
            connector_protocol: program.requirement.connector_protocol.clone(),
            connector_artifact_hash: artifacts.connector_artifact_hash,
            connector_artifact_bytes: artifacts.connector_artifact_bytes,
        }),
        backend,
        account,
        capacity_group,
        credential_generation,
        settings_digest: adapter_settings_digest(&settings)?,
        settings_schema_digest: artifacts.settings_schema_digest,
        settings,
        backend_artifact_hash: artifacts.backend_artifact_hash,
        backend_artifact_bytes: artifacts.backend_artifact_bytes,
        supervisor_artifact_hash: artifacts.supervisor_artifact_hash,
        supervisor_artifact_bytes: artifacts.supervisor_artifact_bytes,
        guest_runtime_manifest_hash,
        runtime_qualification,
        runtime_content_qualification,
        launcher_artifact_hash: artifacts.launcher_artifact_hash,
        launcher_artifact_bytes: artifacts.launcher_artifact_bytes,
        network_policy: network_policy.into(),
        storage_policy: STORAGE_POLICY.into(),
        cleanup_proof: CLEANUP_PROOF.into(),
        provider_network_inputs,
        controller_transport,
        controller_tls_root_certificates_der_base64: tls_roots_der_base64,
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

/// Fresh 256-bit credential generation from the Lillux CSPRNG. Composing
/// again always names a new generation, so an earlier vault entry is never
/// reused or overwritten.
pub fn fresh_credential_generation() -> Result<String> {
    let mut bytes = [0_u8; 32];
    lillux::crypto::fill_random_bytes(&mut bytes)?;
    Ok(hex::encode(bytes))
}

/// Canonical HTTPS origin: scheme, host and optional non-default port only.
pub fn canonical_https_origin(origin: &str) -> Result<String> {
    const REFUSAL: &str =
        "controller origin must be https://host[:port] without path, query, fragment or userinfo";
    let url = url::Url::parse(origin).map_err(|_| anyhow::anyhow!(REFUSAL))?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.port() != Some(0)
            && url.origin().ascii_serialization() == origin,
        REFUSAL
    );
    Ok(origin.to_owned())
}

/// Parse a PEM bundle of trust roots into the sorted, unique canonical
/// base64 DER form the signed binding carries. Only CERTIFICATE blocks are
/// accepted; a key block or any stray text is refused.
pub fn parse_tls_root_pem(pem: &str) -> Result<Vec<String>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    const ONLY_CERTIFICATES: &str = "TLS roots PEM may contain only CERTIFICATE blocks";
    ensure!(
        pem.len() <= MAX_ROOTS_PEM_BYTES,
        "TLS roots PEM exceeds its byte bound"
    );
    let mut certificates = Vec::new();
    let mut open: Option<String> = None;
    for line in pem.lines() {
        let line = line.trim_end_matches('\r');
        match open.as_mut() {
            None => {
                if line == BEGIN {
                    open = Some(String::new());
                } else if !line.trim().is_empty() {
                    bail!(ONLY_CERTIFICATES);
                }
            }
            Some(body) => {
                if line == END {
                    let der = STANDARD
                        .decode(body.as_bytes())
                        .map_err(|_| anyhow::anyhow!("TLS root certificate is not valid base64"))?;
                    // The consumer builds its trust store from exactly these
                    // bytes, so the same parse gates them here: DER that is
                    // not an X.509 certificate (a key, for one) is refused.
                    rustls::RootCertStore::empty()
                        .add(rustls::pki_types::CertificateDer::from(der.as_slice()))
                        .map_err(|_| {
                            anyhow::anyhow!("TLS root certificate is not an X.509 certificate")
                        })?;
                    certificates.push(STANDARD.encode(der));
                    open = None;
                } else {
                    ensure!(
                        line.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')),
                        ONLY_CERTIFICATES
                    );
                    body.push_str(line);
                }
            }
        }
    }
    ensure!(open.is_none(), "TLS roots PEM has an unterminated block");
    ensure!(
        !certificates.is_empty(),
        "TLS roots PEM carries no CERTIFICATE block"
    );
    certificates.sort();
    certificates.dedup();
    ensure!(
        certificates.len() <= MAX_TLS_ROOT_CERTIFICATES,
        "TLS roots PEM carries too many certificates"
    );
    // Digest validation owns the per-certificate and aggregate byte bounds.
    external_tls_root_bundle_digest(&certificates)?;
    Ok(certificates)
}

/// Controller transport an operator selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeTransportRequest {
    Https {
        origin: String,
        roots_pem: String,
    },
    FileRelay {
        owner_workspace_id: String,
        private_mailbox_parent: String,
    },
}

impl ComposeTransportRequest {
    fn label(&self) -> &'static str {
        match self {
            Self::Https { .. } => "https",
            Self::FileRelay { .. } => "file-relay",
        }
    }
}

/// Host adapter contact inputs and the pinned bounds an operator binding uses.
pub(crate) fn operator_network_inputs() -> ExternalNetworkInputPolicy {
    ExternalNetworkInputPolicy {
        resolver: ExternalNetworkInputSelection {
            source: "/etc/resolv.conf".into(),
            max_bytes: 64 * 1024,
        },
        hosts: ExternalNetworkInputSelection {
            source: "/etc/hosts".into(),
            max_bytes: 64 * 1024,
        },
    }
}

pub(super) struct SelectedTransport {
    pub(super) selection: ExternalControllerTransportSelection,
    pub(super) network_inputs: ExternalNetworkInputPolicy,
    pub(super) tls_roots: Vec<String>,
}

pub(super) fn select_transport(
    request: &ComposeTransportRequest,
    settings: &Value,
) -> Result<SelectedTransport> {
    let network_inputs = operator_network_inputs();
    match request {
        ComposeTransportRequest::Https { origin, roots_pem } => {
            let https_origin = canonical_https_origin(origin)?;
            let tls_roots = parse_tls_root_pem(roots_pem)?;
            let contract = ExternalControllerTransportContract {
                schema: 2,
                network_inputs: network_inputs.clone(),
                https_origin,
                route_contract: EXTERNAL_CHANNEL_ROUTE_CONTRACT.into(),
                tls_root_bundle_digest: external_tls_root_bundle_digest(&tls_roots)?,
                connect_timeout_ms: 5_000,
                request_timeout_ms: 10_000,
                maximum_response_bytes: 1024 * 1024,
            };
            Ok(SelectedTransport {
                selection: contract.into(),
                network_inputs,
                tls_roots,
            })
        }
        ComposeTransportRequest::FileRelay {
            owner_workspace_id,
            private_mailbox_parent,
        } => {
            ensure!(
                settings.get("owner_id").and_then(Value::as_str)
                    == Some(owner_workspace_id.as_str()),
                "file-relay settings owner_id must equal the owner workspace id"
            );
            Ok(SelectedTransport {
                selection: ExternalControllerTransportSelection::ControllerFileRelay(
                    ExternalControllerFileRelayContract {
                        schema: 3,
                        route_contract: EXTERNAL_CHANNEL_RELAY_CONTRACT.into(),
                        owner_workspace_id: owner_workspace_id.clone(),
                        private_mailbox_parent: private_mailbox_parent.clone(),
                        maximum_file_bytes: 1024 * 1024,
                        maximum_frames: 1024,
                        poll_interval_ms: 1000,
                        poll_deadline_ms: 30_000,
                    },
                ),
                network_inputs,
                tls_roots: Vec::new(),
            })
        }
    }
}

/// Read the artifact identities for one provider and backend out of the
/// verified installed bundles.
pub(crate) fn bundle_artifact_facts(
    artifacts: &crate::external_artifacts::ResolvedExternalExecutionArtifacts,
    provider_declaration_id: &str,
    backend: &str,
) -> Result<BundleArtifactFacts> {
    let generation = artifacts.placement_backends.installed_generation(backend)?;
    let (configuration_adapter_artifact_hash, configuration_adapter_artifact_bytes) = artifacts
        .provider_configurations
        .coordinate_for_provider(provider_declaration_id)?;
    let connector_hash = artifacts
        .provider_connector_hashes
        .get(provider_declaration_id)
        .context("external provider declaration names no installed connector")?;
    let (connector_artifact_hash, connector_artifact_bytes) =
        artifacts.connectors.coordinate_for_hash(connector_hash)?;
    Ok(BundleArtifactFacts {
        backend_artifact_hash: generation.backend_artifact_hash,
        backend_artifact_bytes: generation.backend_artifact_bytes,
        settings_schema_digest: generation.settings_schema_digest,
        supervisor_artifact_hash: generation.supervisor_artifact_hash,
        supervisor_artifact_bytes: generation.supervisor_artifact_bytes,
        launcher_artifact_hash: generation.launcher_artifact_hash,
        launcher_artifact_bytes: generation.launcher_artifact_bytes,
        configuration_adapter_artifact_hash,
        configuration_adapter_artifact_bytes,
        connector_artifact_hash,
        connector_artifact_bytes,
    })
}

/// Run `read` over a stopped node's installed bundle roots under the
/// bundle-generation lock, so the installed set cannot change during the
/// read. The generation is verified before and after `read` runs.
pub(crate) fn with_installed_bundles<T>(
    app_root: &Path,
    read: impl FnOnce(&[PathBuf], &ryeos_engine::trust::TrustStore, &Path) -> Result<T>,
) -> Result<T> {
    let roots = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf());
    let trust = ryeos_engine::trust::TrustStore::load(None, &roots.config())
        .context("load node trust store")?;
    let _bundle_generation_lock =
        ryeos_node_runtime::registry::BundleRegistryMutationLock::acquire_for_startup(app_root)
            .context("acquire bundle-generation lock")?;
    let records = ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader {
        app_root,
        trust_store: &trust,
    }
    .load_bundle_section()
    .context("load installed bundle registrations")?;
    let generation =
        ryeos_node_runtime::engine_init::VerifiedBundleGeneration::capture(records, &trust)
            .context("capture verified bundle generation")?;
    generation.checked(&trust, || {
        read(
            &generation.roots(),
            &trust,
            &crate::private_artifact_home::root(&roots.state()),
        )
    })
}

/// Resolve the signed artifacts of a stopped node's installed bundles
/// without starting it. Holding the bundle-generation lock keeps the
/// installed set fixed for the whole read.
pub(crate) fn resolve_installed_artifacts(
    app_root: &Path,
) -> Result<crate::external_artifacts::ResolvedExternalExecutionArtifacts> {
    with_installed_bundles(app_root, |bundle_roots, trust, credential_home| {
        crate::external_artifacts::resolve_external_execution_artifacts(
            bundle_roots,
            trust,
            credential_home,
        )
    })
}

/// One operator request to compose and install a binding.
pub struct ComposeRequest<'a> {
    pub app_root: &'a Path,
    /// Proof that the node is stopped; no daemon may read the section while
    /// it is rewritten.
    pub state_lock: &'a ryeos_node_runtime::state_lock::StateLock,
    pub id: String,
    pub backend: String,
    pub program: AdmittedExternalCandidateProgram,
    pub transport: ComposeTransportRequest,
    pub account: String,
    pub capacity_group: String,
    /// The generation the binding names for its provider credential. The
    /// caller chooses it: an interactive operator takes a fresh one, and a
    /// signed offline request derives it from the request digest, so a
    /// re-run of the same request names the generation it already installed.
    pub credential_generation: String,
    pub settings: Value,
    pub guest_runtime_manifest_hash: String,
    pub runtime_qualification: Option<Value>,
    pub runtime_content_qualification: Option<Value>,
    pub limits: ComposeLimits,
}

/// What was installed. It carries no credential and no vault key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposeReport {
    pub id: String,
    pub path: PathBuf,
    pub backend: String,
    pub account: String,
    pub capacity_group: String,
    pub transport: String,
    pub credential_generation: String,
    pub settings_digest: String,
    pub binding_digest: String,
    pub capacity_owner: String,
}

/// Compose, sign with the node identity, verify the exact bytes the loader
/// will read, and publish atomically.
pub fn compose_external_execution_binding(request: ComposeRequest<'_>) -> Result<ComposeReport> {
    compose_with_resolver(request, resolve_installed_artifacts)
}

/// The composition itself, parameterised on how the installed artifacts are
/// resolved so a test can supply registries without a bundle on disk. The
/// resolver runs at the same point of the sequence as the bundle read.
pub(crate) fn compose_with_resolver(
    request: ComposeRequest<'_>,
    resolve: impl FnOnce(&Path) -> Result<crate::external_artifacts::ResolvedExternalExecutionArtifacts>,
) -> Result<ComposeReport> {
    request
        .state_lock
        .ensure_protects_app_root(request.app_root)?;
    validate_document_id(&request.id, "external-execution binding")?;
    let identity = load_node_identity(request.app_root)?;
    let backend = canonical_backend_id(&request.backend).to_owned();
    let transport = select_transport(&request.transport, &request.settings)?;
    let runtime_qualification = request
        .runtime_qualification
        .map(|value| {
            serde_json::from_value::<ExternalRuntimeQualificationBinding>(value)
                .context("invalid runtime qualification")
        })
        .transpose()?;
    let runtime_content_qualification = request
        .runtime_content_qualification
        .map(|value| {
            serde_json::from_value::<ExternalRuntimeContentQualificationBinding>(value)
                .context("invalid runtime content qualification")
        })
        .transpose()?;
    let resolved = resolve(request.app_root)?;
    let artifacts = bundle_artifact_facts(
        &resolved,
        &request.program.requirement.provider_declaration_id,
        &backend,
    )?;
    let document = compose_binding_document(ComposeInputs {
        program: &request.program,
        controller_transport: transport.selection,
        provider_network_inputs: transport.network_inputs,
        tls_roots_der_base64: transport.tls_roots,
        backend,
        account: request.account,
        capacity_group: request.capacity_group,
        credential_generation: request.credential_generation,
        settings: request.settings,
        guest_runtime_manifest_hash: request.guest_runtime_manifest_hash,
        runtime_qualification,
        runtime_content_qualification,
        artifacts,
        limits: request.limits,
    })?;
    let signed = sign_node_document(&serde_yaml::to_string(&document)?, &identity)?;
    let installed = admit_signed_binding(request.app_root, &request.id, &signed, &identity)?;
    let path = publish_signed_document(
        request.app_root,
        SECTION_NAME,
        &request.id,
        signed.as_bytes(),
    )?;
    Ok(ComposeReport {
        id: installed.id.clone(),
        path,
        backend: installed.document.backend.clone(),
        account: installed.document.account.clone(),
        capacity_group: installed.document.capacity_group.clone(),
        transport: request.transport.label().to_owned(),
        credential_generation: installed.document.credential_generation.clone(),
        settings_digest: installed.document.settings_digest.clone(),
        binding_digest: installed.digest.clone(),
        capacity_owner: installed.capacity_owner.clone(),
    })
}

/// The credential generation of the binding installed under `id`, or `None`
/// when no such binding is installed. A binding whose document is unreadable
/// is an error, not an absence.
pub fn installed_binding_credential_generation(
    app_root: &Path,
    id: &str,
) -> Result<Option<String>> {
    validate_document_id(id, "external-execution binding")?;
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
    Ok(Some(
        read_installed_binding(app_root, id, &identity)?
            .document
            .credential_generation,
    ))
}

/// Verify signed bytes exactly as the loader would: a trusted signature
/// whose signer is the current node, then the section's own parse.
pub(crate) fn admit_signed_binding(
    app_root: &Path,
    id: &str,
    signed: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<InstalledExternalExecutionBinding> {
    let body = verify_node_signed(signed, identity, "external-execution binding")?;
    let context = item_context(app_root, SECTION_NAME, id, identity.fingerprint());
    // The section parse is the loader's own gate for this document.
    ExternalExecutionSection.parse(&context, &body)?;
    let document: BindingDocument =
        serde_json::from_value(body).context("invalid external placement binding")?;
    document.validate()?;
    let signer = identity.fingerprint().to_owned();
    Ok(InstalledExternalExecutionBinding {
        id: id.to_owned(),
        digest: binding_digest(id, &signer, signed)?,
        capacity_owner: capacity_owner(&signer, &document)?,
        document,
        signed_source: signed.into(),
        signer,
        signer_verifying_key: identity.verifying_key().to_bytes(),
    })
}

/// Read an installed binding back the way the loader admits it: bounded
/// no-follow read, trusted current-node signature, then the section parse.
pub(crate) fn read_installed_binding(
    app_root: &Path,
    id: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<InstalledExternalExecutionBinding> {
    validate_document_id(id, "external-execution binding")?;
    let signed = read_installed_document(app_root, SECTION_NAME, id)?;
    admit_signed_binding(app_root, id, &signed, identity)
}

/// What a snapshot producer takes from the placement binding it feeds.
#[derive(Debug, Clone)]
pub(crate) struct ProducerSourceFacts {
    pub(crate) backend: String,
    pub(crate) account: String,
    pub(crate) adapter_artifact_hash: String,
    pub(crate) settings: Value,
    pub(crate) network_inputs: ExternalNetworkInputPolicy,
}

impl InstalledExternalExecutionBinding {
    /// Owner, generation, backend and account of the placement credential.
    pub(crate) fn credential_coordinates(&self) -> (String, String, String, String) {
        (
            self.capacity_owner.clone(),
            self.document.credential_generation.clone(),
            self.document.backend.clone(),
            self.document.account.clone(),
        )
    }

    pub(crate) fn producer_source_facts(&self) -> ProducerSourceFacts {
        ProducerSourceFacts {
            backend: self.document.backend.clone(),
            account: self.document.account.clone(),
            adapter_artifact_hash: self.document.backend_artifact_hash.clone(),
            settings: self.document.settings.clone(),
            network_inputs: self.document.provider_network_inputs.clone(),
        }
    }
}

/// Bounded identifier naming one installed node document.
pub(crate) fn validate_document_id(id: &str, what: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && !id.starts_with('.')
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        "{what} id is invalid"
    );
    Ok(())
}

pub(crate) fn load_node_identity(
    app_root: &Path,
) -> Result<ryeos_node_runtime::identity::NodeIdentity> {
    ryeos_node_runtime::identity::NodeIdentity::load(
        &app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/identity/private_key.pem"),
    )
    .context("load node identity")
}

/// Sign `yaml` with the node identity and bound the result.
pub(crate) fn sign_node_document(
    yaml: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<String> {
    let signed = lillux::signature::sign_content_at(
        yaml,
        identity.signing_key(),
        "#",
        None,
        &lillux::time::iso8601_now(),
    );
    ensure!(
        signed.len() as u64 <= ryeos_node_runtime::node_document::MAX_ITEM_BYTES,
        "signed node document exceeds its byte bound"
    );
    Ok(signed)
}

/// Check a trusted signature by the current node and return the body.
pub(crate) fn verify_node_signed(
    signed: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    what: &str,
) -> Result<Value> {
    let envelope = ryeos_engine::contracts::SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let header = ryeos_engine::item_resolution::parse_signature_header(signed, &envelope)
        .with_context(|| format!("{what} has no valid signature line"))?;
    ensure!(
        header.signer_fingerprint == identity.fingerprint(),
        "{what} must be signed by the current node {}; got {}",
        identity.fingerprint(),
        header.signer_fingerprint
    );
    let trust =
        ryeos_engine::trust::TrustStore::from_signers(vec![ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: *identity.verifying_key(),
            label: None,
        }]);
    let (class, _) = ryeos_engine::trust::verify_item_signature(signed, &header, &envelope, &trust)
        .with_context(|| format!("verify {what} signature"))?;
    ensure!(
        class == ryeos_engine::contracts::TrustClass::Trusted,
        "{what} signature is not trusted"
    );
    serde_yaml::from_str(&ryeos_node_runtime::node_config_source::strip_signature(
        signed,
    ))
    .with_context(|| format!("parse {what} body"))
}

pub(crate) fn item_context(
    app_root: &Path,
    section: &str,
    id: &str,
    signer_fingerprint: &str,
) -> crate::node_config::NodeItemContext {
    let rel_path = PathBuf::from(format!("{id}.yaml"));
    crate::node_config::NodeItemContext {
        section: section.into(),
        id: id.to_owned(),
        stem: id.to_owned(),
        source_file: app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join(section)
            .join(&rel_path),
        rel_path,
        signer_fingerprint: signer_fingerprint.to_owned(),
    }
}

/// Bounded no-follow read of one installed section document.
pub(crate) fn read_installed_document(app_root: &Path, section: &str, id: &str) -> Result<String> {
    let path = app_root
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join(section)
        .join(format!("{id}.yaml"));
    let Some(bytes) = lillux::read_optional_regular_file_bounded_no_follow(
        &path,
        ryeos_node_runtime::node_document::MAX_ITEM_BYTES,
    )
    .with_context(|| format!("read {section} document {}", path.display()))?
    else {
        bail!("{section} document `{id}` is not installed");
    };
    String::from_utf8(bytes).with_context(|| format!("{section} document `{id}` is not UTF-8"))
}

/// Atomically publish signed bytes as `<section>/<id>.yaml`.
pub(crate) fn publish_signed_document(
    app_root: &Path,
    section_name: &str,
    id: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    let base =
        lillux::PinnedDirectory::open_or_create(&app_root.join(ryeos_engine::AI_DIR).join("node"))
            .context("establish no-follow node document root")?;
    let section = base
        .open_or_create_child(std::ffi::OsStr::new(section_name), 0o755)
        .with_context(|| format!("establish {section_name} section"))?;
    let _directory_lock = section.lock_exclusive()?;
    let filename = format!("{id}.yaml");
    let filename = std::ffi::OsStr::new(&filename);
    let expected = section.open_pinned_regular(filename, false)?;
    section
        .atomic_write_pinned_if_same(
            filename,
            expected.as_ref(),
            bytes,
            0o600,
            crate::APP_TEMP_PURPOSE,
        )
        .with_context(|| {
            format!(
                "write {section_name} document {}",
                section.path().join(filename).display()
            )
        })?;
    Ok(section.path().join(filename))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const TEST_SIGNER_KEY: [u8; 32] = [91; 32];

    fn identity_in(root: &Path) -> ryeos_node_runtime::identity::NodeIdentity {
        ryeos_node_runtime::identity::NodeIdentity::create(
            &root.join(".ai/node/identity/private_key.pem"),
        )
        .unwrap()
    }

    fn program() -> AdmittedExternalCandidateProgram {
        use ryeos_state::external_execution::admission::{
            CONNECTOR_PROTOCOL, ExternalCandidateExecutionRoute, ExternalCandidateProcFilesystem,
            ExternalCandidateRequirement, ExternalCandidateRuntimeAuthority,
            ExternalCandidateRuntimeRecipe, ExternalCandidateRuntimeSource, PROTOCOL,
        };
        let runtime_recipe = ExternalCandidateRuntimeRecipe {
            schema: 2,
            runtime_mount_destination: "/runtime".into(),
            executable_relative_path: "bin/codex".into(),
            argv0: "codex".into(),
            arguments: vec!["exec-server".into(), "--listen".into(), "stdio".into()],
            cwd: "/workspace".into(),
            environment: std::collections::BTreeMap::new(),
            max_stdout_bytes: 1024 * 1024,
            max_stderr_bytes: 1024 * 1024,
            proc_filesystem: ExternalCandidateProcFilesystem::PidNamespaceNested,
            contain_process_group: false,
            nested_sandbox: true,
        };
        let runtime_recipe_digest = runtime_recipe.digest().unwrap();
        let requirement = ExternalCandidateRequirement {
            schema: 7,
            required_lifecycle_capabilities: Default::default(),
            protocol: PROTOCOL.into(),
            connector_protocol: CONNECTOR_PROTOCOL.into(),
            execution_route: ExternalCandidateExecutionRoute::ConnectorOnly,
            provider_declaration_id: "codex-hosted".into(),
            provider_configuration_destination: "environments.toml".into(),
            runtime_product_declaration_id: "runtime".into(),
            runtime_authority: ExternalCandidateRuntimeAuthority::CapturedProduct,
            runtime_recipe,
        };
        let qualification_use =
            ryeos_state::external_execution::admission::test_support::fixture_qualification_use(
                &requirement,
            )
            .unwrap();
        AdmittedExternalCandidateProgram {
            requirement,
            qualification_use,
            runtime_manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.into(),
            runtime_manifest_hash: "b".repeat(64),
            runtime_source: ExternalCandidateRuntimeSource::CapturedProduct {
                witness_hash: "1".repeat(64),
            },
            qualification_attestation_hash: "2".repeat(64),
            selection_identity_digest: "c".repeat(64),
            runtime_recipe_digest,
        }
    }

    // Two self-signed P-256 certificates and the PKCS#8 key of the first.
    const CERT_A_BODY: &str = concat!(
        "MIIBjjCCATWgAwIBAgIUBulWxSHNplQwE/kkocyS/QefZWQwCgYIKoZIzj0EAwIw\n",
        "HDEaMBgGA1UEAwwRcnllb3MtdGVzdC1yb290LWEwIBcNMjYxMDE1MDg0MzAyWhgP\n",
        "MjEyNjA5MjEwODQzMDJaMBwxGjAYBgNVBAMMEXJ5ZW9zLXRlc3Qtcm9vdC1hMFkw\n",
        "EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAElMH3O+U96At/8bguQghoMw0/EC/HQ+Ir\n",
        "Z2l3l9Be37KcGidJqnoRIY7SqjfvPJt4T8LHPvyJeyE14bvECdkOEKNTMFEwHQYD\n",
        "VR0OBBYEFBTsj4QaaL8THWJUbp06QN3xIZ2YMB8GA1UdIwQYMBaAFBTsj4QaaL8T\n",
        "HWJUbp06QN3xIZ2YMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDRwAwRAIg\n",
        "DMnc0WbVJELUTAdUGldjCKhtJ0xoevzW603Ao0kNROQCIA9LiD3E9qEIGZfsCsmN\n",
        "2xiBg4gDBSA5Bv8Ip5yehlwq\n"
    );
    const CERT_B_BODY: &str = concat!(
        "MIIBjzCCATWgAwIBAgIUPGWI7SyokTvmsPzPAYWeWVtk8lcwCgYIKoZIzj0EAwIw\n",
        "HDEaMBgGA1UEAwwRcnllb3MtdGVzdC1yb290LWIwIBcNMjYxMDE1MDg0MzAyWhgP\n",
        "MjEyNjA5MjEwODQzMDJaMBwxGjAYBgNVBAMMEXJ5ZW9zLXRlc3Qtcm9vdC1iMFkw\n",
        "EwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAESAz+X5Z8c3JC1QvRbpNyIfrhBWKoDw3P\n",
        "ER08ufXq049SS2VIH/6CIDx4jbzYm3CaC86Taswh7i6pg4eNpfom8KNTMFEwHQYD\n",
        "VR0OBBYEFK5pDkvczzbse0aZ68kFyksPyRK+MB8GA1UdIwQYMBaAFK5pDkvczzbs\n",
        "e0aZ68kFyksPyRK+MA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIg\n",
        "S/T6rAvLS9MLxRqZF5XfKTKr/aPijP8qTV2eyc8D1VoCIQC4giVV5cTAPhqD2hqy\n",
        "H6io4TaaRBC8yhOtuJHnmQ0DQg==\n"
    );
    const KEY_A_BODY: &str = concat!(
        "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgi3tYG8836U5RN6KU\n",
        "nr7KYVz4JtMrB4QDcF48KCCV8IGhRANCAASUwfc75T3oC3/xuC5CCGgzDT8QL8dD\n",
        "4itnaXeX0F7fspwaJ0mqehEhjtKqN+88m3hPwsc+/Il7ITXhu8QJ2Q4Q\n"
    );

    fn pem_block(label: &str, body: &str) -> String {
        format!("-----BEGIN {label}-----\n{body}-----END {label}-----\n")
    }

    pub(crate) fn cert_a() -> String {
        pem_block("CERTIFICATE", CERT_A_BODY)
    }

    fn cert_b() -> String {
        pem_block("CERTIFICATE", CERT_B_BODY)
    }

    fn der_of(body: &str) -> Vec<u8> {
        STANDARD.decode(body.replace('\n', "")).unwrap()
    }

    pub(crate) fn root_pem() -> String {
        format!("{}{}", cert_a(), cert_b())
    }

    fn facts() -> BundleArtifactFacts {
        BundleArtifactFacts {
            backend_artifact_hash: "d".repeat(64),
            backend_artifact_bytes: 4096,
            settings_schema_digest: "3".repeat(64),
            supervisor_artifact_hash: "1".repeat(64),
            supervisor_artifact_bytes: 4096,
            launcher_artifact_hash: "e".repeat(64),
            launcher_artifact_bytes: 4096,
            configuration_adapter_artifact_hash: "2".repeat(64),
            configuration_adapter_artifact_bytes: 4096,
            connector_artifact_hash: "f".repeat(64),
            connector_artifact_bytes: 4096,
        }
    }

    fn https() -> ComposeTransportRequest {
        ComposeTransportRequest::Https {
            origin: "https://controller.example:7443".into(),
            roots_pem: root_pem(),
        }
    }

    fn relay() -> ComposeTransportRequest {
        ComposeTransportRequest::FileRelay {
            owner_workspace_id: "render-owner".into(),
            private_mailbox_parent: "/run/ryeos/private-relay".into(),
        }
    }

    fn compose(
        program: &AdmittedExternalCandidateProgram,
        transport: &ComposeTransportRequest,
        settings: Value,
    ) -> Result<BindingDocument> {
        let selected = select_transport(transport, &settings)?;
        compose_binding_document(ComposeInputs {
            program,
            controller_transport: selected.selection,
            provider_network_inputs: selected.network_inputs,
            tls_roots_der_base64: selected.tls_roots,
            backend: RENDER_BACKEND_ID.into(),
            account: "operator-account".into(),
            capacity_group: DEFAULT_CAPACITY_GROUP.into(),
            credential_generation: fresh_credential_generation()?,
            settings,
            guest_runtime_manifest_hash: "4".repeat(64),
            runtime_qualification: None,
            runtime_content_qualification: None,
            artifacts: facts(),
            limits: ComposeLimits::default(),
        })
    }

    fn refusal<T>(result: Result<T>) -> anyhow::Error {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected a refusal"),
        }
    }

    fn relay_settings() -> Value {
        serde_json::json!({"owner_id": "render-owner"})
    }

    fn content_program() -> AdmittedExternalCandidateProgram {
        ryeos_state::external_execution::admission::test_support::content_candidate_capsule(
            &"b".repeat(64),
        )
        .unwrap()
        .external_candidate
        .unwrap()
    }

    fn content_qualification_value() -> Value {
        let program = content_program();
        let ExternalCandidateRuntimeSource::ActivatedContent {
            qualification_coordinate_id,
            ..
        } = &program.runtime_source
        else {
            panic!("content fixture program is activated content");
        };
        serde_json::json!({
            "activation_ref": "config:fixtures/runtime-activation",
            "coordinate_id": qualification_coordinate_id,
            "attestation_hash": program.qualification_attestation_hash,
            "owner_principal": format!("fp:{}", "7".repeat(64)),
            "required_claims": ["compatible"],
            "remote_testimony": Value::Null,
        })
    }

    fn content_qualification() -> ExternalRuntimeContentQualificationBinding {
        serde_json::from_value(content_qualification_value()).unwrap()
    }

    fn compose_with_content(
        program: &AdmittedExternalCandidateProgram,
        content: Option<ExternalRuntimeContentQualificationBinding>,
    ) -> Result<BindingDocument> {
        let selected = select_transport(&relay(), &relay_settings())?;
        compose_binding_document(ComposeInputs {
            program,
            controller_transport: selected.selection,
            provider_network_inputs: selected.network_inputs,
            tls_roots_der_base64: selected.tls_roots,
            backend: RENDER_BACKEND_ID.into(),
            account: "operator-account".into(),
            capacity_group: DEFAULT_CAPACITY_GROUP.into(),
            credential_generation: "a".repeat(64),
            settings: relay_settings(),
            guest_runtime_manifest_hash: "4".repeat(64),
            runtime_qualification: None,
            runtime_content_qualification: content,
            artifacts: facts(),
            limits: ComposeLimits::default(),
        })
    }

    #[test]
    fn content_qualification_lands_in_the_document_and_changes_its_digest() {
        let program = content_program();
        let with = compose_with_content(&program, Some(content_qualification())).unwrap();
        let mut narrower = content_qualification();
        narrower.required_claims = vec!["compatible".into(), "licensed".into()];
        let other = compose_with_content(&program, Some(narrower.clone())).unwrap();
        assert_eq!(
            with.runtime_content_qualification,
            Some(content_qualification())
        );
        assert_eq!(other.runtime_content_qualification, Some(narrower));
        let digest = |document: &BindingDocument| {
            binding_digest(
                "operator",
                "signer",
                &serde_yaml::to_string(document).unwrap(),
            )
            .unwrap()
        };
        assert_ne!(digest(&with), digest(&other));
    }

    #[test]
    fn a_content_qualification_for_another_coordinate_is_refused() {
        let program = content_program();
        let mut other = content_qualification();
        other.coordinate_id = "5".repeat(64);
        let error = refusal(compose_with_content(&program, Some(other)));
        assert!(
            format!("{error:#}").contains("coordinate the program was not derived from"),
            "{error:#}"
        );
    }

    #[test]
    fn a_content_qualification_for_another_attestation_is_refused() {
        let program = content_program();
        let mut other = content_qualification();
        other.attestation_hash = "6".repeat(64);
        let error = refusal(compose_with_content(&program, Some(other)));
        assert!(
            format!("{error:#}").contains("attestation the program was not derived from"),
            "{error:#}"
        );
    }

    #[test]
    fn a_captured_product_program_carries_a_content_qualification_as_session_testimony() {
        let node = stopped_node();
        let mut request = compose_request(&node, "render");
        request.runtime_content_qualification = Some(content_qualification_value());
        let resolved =
            installed_artifacts(&node, Some(PROVIDER_ID), vec![lifecycle(&"d".repeat(64))]);
        let report = compose_with_resolver(request, |_| Ok(resolved)).unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &node.root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let signed = std::fs::read_to_string(&report.path).unwrap();
        let loaded =
            admit_signed_binding(node.root.path(), BINDING_ID, &signed, &identity).unwrap();
        assert_eq!(
            loaded.document.runtime_content_qualification,
            Some(content_qualification())
        );
    }

    #[test]
    fn a_content_qualification_selection_states_its_remote_testimony_explicitly() {
        let local = content_qualification_value();
        assert!(content_qualification().remote_testimony.is_none());

        let mut remote = local.clone();
        remote["remote_testimony"] = serde_json::json!({
            "acceptance_hash": "8".repeat(64),
            "acceptance_policy_ref": "config:fixtures/content-acceptance",
        });
        let parsed: ExternalRuntimeContentQualificationBinding =
            serde_json::from_value(remote).unwrap();
        parsed.validate().unwrap();
        assert_eq!(
            parsed.remote_testimony.unwrap().acceptance_policy_ref,
            "config:fixtures/content-acceptance"
        );

        let mut missing = local;
        missing.as_object_mut().unwrap().remove("remote_testimony");
        assert!(
            serde_json::from_value::<ExternalRuntimeContentQualificationBinding>(missing).is_err()
        );
    }

    #[test]
    fn a_remote_content_testimony_selection_is_signed_through_composition() {
        let node = stopped_node();
        let mut request = compose_request(&node, "render");
        let mut selected = content_qualification_value();
        selected["remote_testimony"] = serde_json::json!({
            "acceptance_hash": "8".repeat(64),
            "acceptance_policy_ref": "config:fixtures/content-acceptance",
        });
        request.runtime_content_qualification = Some(selected);
        let resolved =
            installed_artifacts(&node, Some(PROVIDER_ID), vec![lifecycle(&"d".repeat(64))]);
        let report = compose_with_resolver(request, |_| Ok(resolved)).unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &node.root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let signed = std::fs::read_to_string(&report.path).unwrap();
        let loaded =
            admit_signed_binding(node.root.path(), BINDING_ID, &signed, &identity).unwrap();
        let selected = loaded.document.runtime_content_qualification.unwrap();
        assert_eq!(
            selected.remote_testimony.unwrap().acceptance_hash,
            "8".repeat(64)
        );
    }

    fn guest_qualification_value(remote: Value) -> Value {
        serde_json::json!({
            "attestation_hash": "6".repeat(64),
            "owner_principal": format!("fp:{}", "7".repeat(64)),
            "qualification": {
                "policy_ref": "config:fixtures/guest-qualification",
                "required_claims": ["render_snapshot_v1"],
            },
            "remote_testimony": remote,
        })
    }

    #[test]
    fn a_guest_qualification_selection_states_its_remote_testimony_explicitly() {
        let local = guest_qualification_value(Value::Null);
        let parsed: ExternalRuntimeQualificationBinding =
            serde_json::from_value(local.clone()).unwrap();
        assert!(parsed.remote_testimony.is_none());

        let remote = guest_qualification_value(serde_json::json!({
            "acceptance_hash": "8".repeat(64),
            "acceptance_policy_ref": "config:fixtures/guest-acceptance",
        }));
        let parsed: ExternalRuntimeQualificationBinding = serde_json::from_value(remote).unwrap();
        parsed.validate().unwrap();
        assert_eq!(
            parsed.remote_testimony.unwrap().acceptance_policy_ref,
            "config:fixtures/guest-acceptance"
        );

        let mut missing = local;
        missing.as_object_mut().unwrap().remove("remote_testimony");
        assert!(serde_json::from_value::<ExternalRuntimeQualificationBinding>(missing).is_err());
    }

    #[test]
    fn a_remote_testimony_selection_is_signed_through_composition() {
        let node = stopped_node();
        let mut request = compose_request(&node, "render");
        request.runtime_qualification = Some(guest_qualification_value(serde_json::json!({
            "acceptance_hash": "8".repeat(64),
            "acceptance_policy_ref": "config:fixtures/guest-acceptance",
        })));
        let resolved =
            installed_artifacts(&node, Some(PROVIDER_ID), vec![lifecycle(&"d".repeat(64))]);
        let report = compose_with_resolver(request, |_| Ok(resolved)).unwrap();
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &node.root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let signed = std::fs::read_to_string(&report.path).unwrap();
        let loaded =
            admit_signed_binding(node.root.path(), BINDING_ID, &signed, &identity).unwrap();
        let selected = loaded.document.runtime_qualification.unwrap();
        assert_eq!(
            selected.remote_testimony.unwrap().acceptance_hash,
            "8".repeat(64)
        );
    }

    #[test]
    fn an_activated_content_program_is_refused_without_a_content_qualification() {
        let node = stopped_node();
        let mut request = compose_request(&node, "render");
        request.program = content_program();
        let resolved =
            installed_artifacts(&node, Some(PROVIDER_ID), vec![lifecycle(&"d".repeat(64))]);
        let error = refusal(compose_with_resolver(request, |_| Ok(resolved)));
        assert!(
            format!("{error:#}").contains("requires a runtime content qualification"),
            "{error:#}"
        );
        assert!(!binding_path(&node).exists());
    }

    #[test]
    fn a_content_qualification_with_a_bad_activation_ref_is_refused() {
        let program = content_program();
        let mut bad = content_qualification();
        bad.activation_ref = "tool:fixtures/runtime-activation".into();
        let error = refusal(compose_with_content(&program, Some(bad)));
        assert!(
            format!("{error:#}").contains("exact activation config ref"),
            "{error:#}"
        );
    }

    #[test]
    fn both_transports_validate_and_pick_their_schema_and_network_policy() {
        let program = program();
        let http = compose(&program, &https(), serde_json::json!({"region": "r"})).unwrap();
        assert_eq!(
            (http.schema, http.network_policy.as_str()),
            (17, HTTP_NETWORK_POLICY)
        );
        assert_eq!(http.controller_tls_root_certificates_der_base64.len(), 2);
        let file = compose(&program, &relay(), relay_settings()).unwrap();
        assert_eq!(
            (file.schema, file.network_policy.as_str()),
            (19, RELAY_NETWORK_POLICY)
        );
        assert!(file.controller_tls_root_certificates_der_base64.is_empty());
    }

    #[test]
    fn two_composes_differ_in_credential_generation_only() {
        let program = program();
        let first = compose(&program, &https(), serde_json::json!({})).unwrap();
        let second = compose(&program, &https(), serde_json::json!({})).unwrap();
        assert_ne!(first.credential_generation, second.credential_generation);
        assert_eq!(first.credential_generation.len(), 64);
        let mut aligned = second.clone();
        aligned.credential_generation = first.credential_generation.clone();
        assert!(first == aligned);
    }

    #[test]
    fn artifact_hashes_and_workload_come_from_the_inputs_not_defaults() {
        let program = program();
        let document = compose(&program, &https(), serde_json::json!({})).unwrap();
        let facts = facts();
        assert_eq!(document.backend_artifact_hash, facts.backend_artifact_hash);
        assert_eq!(
            document.settings_schema_digest,
            facts.settings_schema_digest
        );
        assert_eq!(
            document.supervisor_artifact_hash,
            facts.supervisor_artifact_hash
        );
        assert_eq!(
            document.launcher_artifact_hash,
            facts.launcher_artifact_hash
        );
        let session = document.workload.structured_session().unwrap();
        assert_eq!(
            session.configuration_adapter_artifact_hash,
            facts.configuration_adapter_artifact_hash
        );
        assert_eq!(
            session.connector_artifact_hash,
            facts.connector_artifact_hash
        );
        assert_eq!(session.runtime_manifest_hash, program.runtime_manifest_hash);
        assert_eq!(
            session.runtime_selection_identity,
            program.selection_identity_digest
        );
    }

    #[test]
    fn origin_must_be_a_bare_https_origin() {
        assert_eq!(
            canonical_https_origin("https://controller.example:7443").unwrap(),
            "https://controller.example:7443"
        );
        for bad in [
            "http://controller.example",
            "https://controller.example/",
            "https://controller.example/path",
            "https://controller.example?x=1",
            "https://controller.example#f",
            "https://user@controller.example",
            "https://user:pw@controller.example",
            "controller.example",
            "",
            "https://controller.example:443",
            "https://Controller.Example:7443",
            "https://controller.example:7443/",
            " https://controller.example:7443",
            "https://controller.example:7443 ",
            "\thttps://controller.example:7443\n",
            "https://b\u{fc}cher.example:7443",
            "https://controller.example:0",
            "https://controller.example:65536",
        ] {
            let error = canonical_https_origin(bad).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("without path, query, fragment or userinfo"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn ip_literal_origins_are_accepted_in_canonical_form() {
        for good in ["https://192.0.2.1:7443", "https://[::1]:7443"] {
            assert_eq!(canonical_https_origin(good).unwrap(), good);
        }
        for bad in [
            "https://[::1]:443",
            "https://[0:0:0:0:0:0:0:1]:7443",
            "https://192.0.2.1:0",
        ] {
            assert!(canonical_https_origin(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn compose_against_a_node_without_installed_bundles_publishes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let _identity = identity_in(root.path());
        let lock = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(root.path()),
        )
        .unwrap();
        let error = refusal(compose_external_execution_binding(ComposeRequest {
            app_root: root.path(),
            state_lock: &lock,
            id: "operator".into(),
            backend: "render".into(),
            program: program(),
            transport: https(),
            account: "operator-account".into(),
            capacity_group: DEFAULT_CAPACITY_GROUP.into(),
            credential_generation: fresh_credential_generation().unwrap(),
            settings: serde_json::json!({}),
            guest_runtime_manifest_hash: "4".repeat(64),
            runtime_qualification: None,
            runtime_content_qualification: None,
            limits: ComposeLimits::default(),
        }));
        assert!(
            format!("{error:#}").contains("load installed bundle registrations"),
            "{error:#}"
        );
        assert!(
            !root
                .path()
                .join(".ai/node/external_execution/operator.yaml")
                .exists()
        );
    }

    #[test]
    fn roots_must_be_certificates_only_and_nonempty() {
        for bad in [
            "".to_owned(),
            "\n\n".to_owned(),
            // Key-shaped DER under a CERTIFICATE label.
            pem_block("CERTIFICATE", KEY_A_BODY),
            // A well-formed DER sequence that is not a certificate.
            pem_block("CERTIFICATE", "MAMCAQE=\n"),
            "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n".to_owned(),
            format!(
                "{}-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
                root_pem()
            ),
            format!("stray\n{}", root_pem()),
            "-----BEGIN CERTIFICATE-----\nMAMCAQE=\n".to_owned(),
            "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n".to_owned(),
        ] {
            assert!(parse_tls_root_pem(&bad).is_err(), "{bad:?}");
        }
        let program = program();
        let empty = ComposeTransportRequest::Https {
            origin: "https://controller.example".into(),
            roots_pem: String::new(),
        };
        assert!(compose(&program, &empty, serde_json::json!({})).is_err());
    }

    #[test]
    fn a_real_certificate_is_accepted_and_keeps_its_der() {
        let roots = parse_tls_root_pem(&cert_a()).unwrap();
        assert_eq!(roots, vec![STANDARD.encode(der_of(CERT_A_BODY))]);
        let error = parse_tls_root_pem(&pem_block("CERTIFICATE", KEY_A_BODY)).unwrap_err();
        assert!(
            error.to_string().contains("not an X.509 certificate"),
            "{error}"
        );
    }

    #[test]
    fn roots_digest_matches_the_signed_transport_and_ordering_is_canonical() {
        let forward = parse_tls_root_pem(&format!("{}{}", cert_b(), cert_a())).unwrap();
        let reverse = parse_tls_root_pem(&root_pem()).unwrap();
        assert_eq!(forward, reverse);
        let document = compose(&program(), &https(), serde_json::json!({})).unwrap();
        let http = document.controller_transport.http().unwrap();
        assert_eq!(
            http.tls_root_bundle_digest,
            external_tls_root_bundle_digest(&forward).unwrap()
        );
    }

    #[test]
    fn relay_owner_must_equal_settings_owner_and_carries_no_roots() {
        let program = program();
        let error = refusal(compose(
            &program,
            &relay(),
            serde_json::json!({"owner_id": "other"}),
        ));
        assert!(
            error
                .to_string()
                .contains("owner_id must equal the owner workspace id"),
            "{error}"
        );
        assert!(compose(&program, &relay(), serde_json::json!({})).is_err());
        // The constructor itself refuses roots beside a relay transport.
        let selected = select_transport(&relay(), &relay_settings()).unwrap();
        let error = refusal(compose_binding_document(ComposeInputs {
            program: &program,
            controller_transport: selected.selection,
            provider_network_inputs: selected.network_inputs,
            tls_roots_der_base64: vec![STANDARD.encode(der_of(CERT_A_BODY))],
            backend: RENDER_BACKEND_ID.into(),
            account: "a".into(),
            capacity_group: DEFAULT_CAPACITY_GROUP.into(),
            credential_generation: "a".repeat(64),
            settings: relay_settings(),
            guest_runtime_manifest_hash: "4".repeat(64),
            runtime_qualification: None,
            runtime_content_qualification: None,
            artifacts: facts(),
            limits: ComposeLimits::default(),
        }));
        assert!(
            error.to_string().contains("carries no TLS roots"),
            "{error}"
        );
    }

    #[test]
    fn render_alias_resolves_and_other_backends_pass_through() {
        assert_eq!(canonical_backend_id("render"), RENDER_BACKEND_ID);
        assert_eq!(canonical_backend_id(RENDER_BACKEND_ID), RENDER_BACKEND_ID);
        assert_eq!(canonical_backend_id("other"), "other");
    }

    #[test]
    fn signed_binding_round_trips_and_a_foreign_signer_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let identity = identity_in(root.path());
        let document = compose(&program(), &https(), serde_json::json!({"region": "r"})).unwrap();
        let sign = |key: &lillux::crypto::SigningKey| {
            lillux::signature::sign_content_at(
                &serde_yaml::to_string(&document).unwrap(),
                key,
                "#",
                None,
                "2026-10-15T00:00:00Z",
            )
        };
        let signed = sign(identity.signing_key());
        let installed = admit_signed_binding(root.path(), "operator", &signed, &identity).unwrap();
        assert_eq!(installed.id(), "operator");
        assert!(installed.document == document);
        assert_eq!(installed.signer, identity.fingerprint());
        installed.retained_generation().unwrap();

        let foreign = lillux::crypto::SigningKey::from_bytes(&TEST_SIGNER_KEY);
        let error =
            admit_signed_binding(root.path(), "operator", &sign(&foreign), &identity).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must be signed by the current node"),
            "{error}"
        );
    }

    #[test]
    fn published_file_is_replaced_atomically_and_carries_the_signed_bytes() {
        let root = tempfile::tempdir().unwrap();
        let first =
            publish_signed_document(root.path(), SECTION_NAME, "operator", b"first").unwrap();
        assert_eq!(
            first,
            root.path()
                .join(".ai/node/external_execution/operator.yaml")
        );
        let second =
            publish_signed_document(root.path(), SECTION_NAME, "operator", b"second").unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
    }

    #[test]
    fn binding_id_is_a_bounded_identifier() {
        assert!(validate_document_id("render-1.a_b", "binding").is_ok());
        for bad in ["", ".hidden", "a/b", "a b", &"a".repeat(129)] {
            assert!(validate_document_id(bad, "binding").is_err(), "{bad}");
        }
    }

    #[test]
    fn limits_file_overrides_named_fields_and_refuses_unknown_ones() {
        let limits: ComposeLimits =
            serde_json::from_value(serde_json::json!({"max_active": 3})).unwrap();
        assert_eq!(limits.max_active, 3);
        assert_eq!(
            limits.timeout_seconds,
            ComposeLimits::default().timeout_seconds
        );
        assert!(
            serde_json::from_value::<ComposeLimits>(serde_json::json!({"max_actve": 3})).is_err()
        );
    }

    use crate::external_placement::compose_test_support as installed;

    const PROVIDER_ID: &str = "codex-hosted";
    const BINDING_ID: &str = "operator";
    const SETTINGS_SCHEMA: &str =
        "3333333333333333333333333333333333333333333333333333333333333333";

    /// A stopped node with an identity, a vault key pair, the state lock and
    /// real connector and configuration-adapter executables.
    struct StoppedNode {
        root: tempfile::TempDir,
        lock: ryeos_node_runtime::state_lock::StateLock,
        connector: PathBuf,
        adapter: PathBuf,
    }

    fn stopped_node() -> StoppedNode {
        let root = tempfile::tempdir().unwrap();
        let _identity = identity_in(root.path());
        let key = lillux::vault::VaultSecretKey::generate();
        lillux::vault::write_secret_key(
            &ryeos_vault::paths::default_vault_secret_key_path(root.path()),
            &key,
        )
        .unwrap();
        lillux::vault::write_public_key(
            &ryeos_vault::paths::default_vault_public_key_path(root.path()),
            &key.public_key(),
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

    fn provider_declaration(
        id: &str,
    ) -> ryeos_external_execution_contract::ExternalProviderDeclaration {
        ryeos_external_execution_contract::ExternalProviderDeclaration {
            id: id.to_owned(),
            protocol: ryeos_external_execution_contract::PROVIDER_DECLARATION_PROTOCOL.to_owned(),
            targets: vec!["x86_64-unknown-linux-gnu".to_owned()],
            connector: "provider-connector".to_owned(),
            connector_process_group:
                ryeos_external_execution_contract::ExternalProviderConnectorProcessGroup::Inherited,
            configuration_adapter: "provider-configuration".to_owned(),
            configuration_destination: "environments.toml".to_owned(),
            qualification_request_binding_protocol: None,
        }
    }

    fn lifecycle(
        artifact: &str,
    ) -> std::sync::Arc<dyn crate::external_placement::ExternalPlacementBackend> {
        installed::CoordinateOnlyBackend::shared(
            RENDER_BACKEND_ID,
            artifact,
            SETTINGS_SCHEMA,
            &"1".repeat(64),
            &"e".repeat(64),
        )
    }

    /// Registries holding `provider` over the node's executables, and the
    /// given lifecycle generations.
    fn installed_artifacts(
        node: &StoppedNode,
        provider: Option<&str>,
        backends: Vec<std::sync::Arc<dyn crate::external_placement::ExternalPlacementBackend>>,
    ) -> crate::external_artifacts::ResolvedExternalExecutionArtifacts {
        let mut provider_connector_hashes = std::collections::BTreeMap::new();
        let mut configurations = Vec::new();
        if let Some(provider) = provider {
            let (connector_hash, _) = installed::connector_coordinate(&node.connector);
            provider_connector_hashes.insert(provider.to_owned(), connector_hash);
            configurations.push(
                installed::provider_configuration(provider_declaration(provider), &node.adapter)
                    .unwrap(),
            );
        }
        crate::external_artifacts::ResolvedExternalExecutionArtifacts {
            provider_connector_hashes,
            connectors: installed::connector_registry(provider.map(|_| node.connector.clone()))
                .unwrap(),
            provider_configurations:
                crate::external_placement::ExternalProviderConfigurationRegistry::from_artifacts(
                    configurations,
                )
                .unwrap(),
            placement_backends:
                crate::external_placement::ExternalPlacementBackendRegistry::from_backends(backends)
                    .unwrap(),
        }
    }

    fn compose_request<'a>(node: &'a StoppedNode, backend: &str) -> ComposeRequest<'a> {
        ComposeRequest {
            app_root: node.root.path(),
            state_lock: &node.lock,
            id: BINDING_ID.into(),
            backend: backend.into(),
            program: program(),
            transport: https(),
            account: "operator-account".into(),
            capacity_group: DEFAULT_CAPACITY_GROUP.into(),
            credential_generation: fresh_credential_generation().unwrap(),
            settings: serde_json::json!({}),
            guest_runtime_manifest_hash: "4".repeat(64),
            runtime_qualification: None,
            runtime_content_qualification: None,
            limits: ComposeLimits::default(),
        }
    }

    fn binding_path(node: &StoppedNode) -> PathBuf {
        node.root
            .path()
            .join(".ai/node/external_execution/operator.yaml")
    }

    #[test]
    fn composed_binding_takes_its_artifact_facts_from_the_installed_registries() {
        let node = stopped_node();
        let resolved =
            installed_artifacts(&node, Some(PROVIDER_ID), vec![lifecycle(&"d".repeat(64))]);
        let report =
            compose_with_resolver(compose_request(&node, "render"), |_| Ok(resolved)).unwrap();

        assert_eq!(report.id, BINDING_ID);
        assert_eq!(report.backend, RENDER_BACKEND_ID);
        assert_eq!(report.path, binding_path(&node));
        let published =
            lillux::secure_fs::open_pinned_regular_file_no_follow(&report.path).unwrap();
        assert_eq!(
            published.observation().unwrap().permission_mode().unwrap(),
            0o600
        );

        // The published bytes load back through the loader's own admission
        // under the node identity.
        let identity = ryeos_node_runtime::identity::NodeIdentity::load(
            &node.root.path().join(".ai/node/identity/private_key.pem"),
        )
        .unwrap();
        let signed = std::fs::read_to_string(&report.path).unwrap();
        let loaded =
            admit_signed_binding(node.root.path(), BINDING_ID, &signed, &identity).unwrap();
        loaded.retained_generation().unwrap();
        assert_eq!(loaded.digest, report.binding_digest);
        assert_eq!(loaded.capacity_owner, report.capacity_owner);
        assert_eq!(
            loaded.document.credential_generation,
            report.credential_generation
        );
        assert_eq!(loaded.document.settings_digest, report.settings_digest);

        let document = &loaded.document;
        assert_eq!(document.settings_schema_digest, SETTINGS_SCHEMA);
        assert_eq!(document.backend_artifact_hash, "d".repeat(64));
        assert_eq!(document.backend_artifact_bytes, 4096);
        assert_eq!(document.supervisor_artifact_hash, "1".repeat(64));
        assert_eq!(document.supervisor_artifact_bytes, 8192);
        assert_eq!(document.launcher_artifact_hash, "e".repeat(64));
        assert_eq!(document.launcher_artifact_bytes, 12288);
        let (connector_hash, connector_bytes) = installed::connector_coordinate(&node.connector);
        let (adapter_hash, adapter_bytes) =
            installed::provider_configuration_coordinate(&node.adapter);
        let session = document.workload.structured_session().unwrap();
        assert_eq!(session.connector_artifact_hash, connector_hash);
        assert_eq!(session.connector_artifact_bytes, connector_bytes);
        assert_eq!(session.configuration_adapter_artifact_hash, adapter_hash);
        assert_eq!(session.configuration_adapter_artifact_bytes, adapter_bytes);
    }

    #[test]
    fn composing_for_an_uninstalled_provider_publishes_nothing() {
        let node = stopped_node();
        let resolved = installed_artifacts(
            &node,
            Some("another-provider"),
            vec![lifecycle(&"d".repeat(64))],
        );
        let error = refusal(compose_with_resolver(
            compose_request(&node, "render"),
            |_| Ok(resolved),
        ));
        assert!(
            format!("{error:#}").contains("external provider declaration is not installed"),
            "{error:#}"
        );
        assert!(!binding_path(&node).exists());
    }

    #[test]
    fn composing_for_an_uninstalled_backend_publishes_nothing() {
        let node = stopped_node();
        let resolved = installed_artifacts(&node, Some(PROVIDER_ID), Vec::new());
        let error = refusal(compose_with_resolver(
            compose_request(&node, "render"),
            |_| Ok(resolved),
        ));
        assert!(
            format!("{error:#}").contains(
                "external lifecycle backend `render-sandbox-early-access` is not installed"
            ),
            "{error:#}"
        );
        assert!(!binding_path(&node).exists());
    }

    #[test]
    fn artifact_lookups_refuse_the_absent_and_ambiguous_cases() {
        let node = stopped_node();
        let facts_error =
            |resolved: &crate::external_artifacts::ResolvedExternalExecutionArtifacts| {
                format!(
                    "{:#}",
                    refusal(bundle_artifact_facts(
                        resolved,
                        PROVIDER_ID,
                        RENDER_BACKEND_ID
                    ))
                )
            };

        // Two installed generations under one backend id are ambiguous.
        let two_generations = installed_artifacts(
            &node,
            Some(PROVIDER_ID),
            vec![lifecycle(&"d".repeat(64)), lifecycle(&"a".repeat(64))],
        );
        assert!(
            facts_error(&two_generations).contains("has more than one installed generation"),
            "ambiguous lifecycle generation"
        );

        // A provider with no installed adapter, or with no connector
        // declared for it, or whose connector is not installed.
        let mut resolved =
            installed_artifacts(&node, Some(PROVIDER_ID), vec![lifecycle(&"d".repeat(64))]);
        resolved.provider_connector_hashes.clear();
        assert!(
            facts_error(&resolved).contains("names no installed connector"),
            "provider without a declared connector"
        );
        resolved
            .provider_connector_hashes
            .insert(PROVIDER_ID.to_owned(), "9".repeat(64));
        assert!(
            facts_error(&resolved).contains("installed external candidate connector is absent"),
            "connector absent"
        );
        let no_provider = installed_artifacts(&node, None, vec![lifecycle(&"d".repeat(64))]);
        assert!(
            facts_error(&no_provider).contains("external provider declaration is not installed"),
            "provider absent"
        );
        let no_backend = installed_artifacts(&node, Some(PROVIDER_ID), Vec::new());
        assert!(
            facts_error(&no_backend).contains("is not installed"),
            "backend absent"
        );
    }

    #[test]
    fn duplicate_provider_and_connector_identities_cannot_be_installed() {
        // The registries refuse a duplicate at construction, so the
        // ambiguous provider and connector lookups are unreachable from any
        // installed set; this pins the refusals that guarantee it.
        let node = stopped_node();
        let twin = node.root.path().join("connector-twin");
        installed::write_executable(&twin, b"connector executable bytes");
        let error = installed::connector_registry([node.connector.clone(), twin]).unwrap_err();
        assert!(error.to_string().contains("is duplicated"), "{error:#}");

        let duplicate =
            crate::external_placement::ExternalProviderConfigurationRegistry::from_artifacts(vec![
                installed::provider_configuration(provider_declaration(PROVIDER_ID), &node.adapter)
                    .unwrap(),
                installed::provider_configuration(provider_declaration(PROVIDER_ID), &node.adapter)
                    .unwrap(),
            ])
            .unwrap_err();
        assert!(
            duplicate.to_string().contains("is duplicated"),
            "{duplicate:#}"
        );
    }

    #[test]
    fn a_composed_binding_provisions_a_credential_the_vault_returns() {
        use crate::vault::{NodeVault as _, SealedEnvelopeVault};

        let node = stopped_node();
        let resolved =
            installed_artifacts(&node, Some(PROVIDER_ID), vec![lifecycle(&"d".repeat(64))]);
        let report =
            compose_with_resolver(compose_request(&node, "render"), |_| Ok(resolved)).unwrap();

        let provisioned = super::super::provision::provision_installed_placement_credential(
            node.root.path(),
            BINDING_ID,
            &zeroize::Zeroizing::new("provider-secret".to_owned()),
        )
        .unwrap();
        assert_eq!(
            provisioned.outcome,
            super::super::provision::ProvisionOutcome::Inserted
        );
        assert_eq!(provisioned.owner, report.capacity_owner);
        assert_eq!(provisioned.generation, report.credential_generation);
        assert_eq!(provisioned.backend, report.backend);
        assert_eq!(provisioned.account, report.account);

        let access = crate::vault::placement::PlacementCredentialAccess::new(
            &report.capacity_owner,
            &report.credential_generation,
            &report.backend,
            &report.account,
        )
        .unwrap();
        let vault = SealedEnvelopeVault::load(node.root.path()).unwrap();
        let decoded = access
            .decode(vault.placement_credential(&access).unwrap())
            .unwrap();
        assert_eq!(decoded.secret(), "provider-secret");
    }
}
