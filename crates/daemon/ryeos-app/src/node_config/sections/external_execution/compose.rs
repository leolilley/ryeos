//! Composition of one node-signed external placement binding.
//!
//! `compose_binding_document` is the only constructor of a binding document
//! outside the loader. Test fixtures and the offline operator command both
//! call it, so a fixture can never drift from what an operator composes. The
//! operator path adds the facts only an installed node can supply (artifact
//! identities from the verified bundles), a fresh credential generation, the
//! node-identity signature and an exact round trip through the section parse
//! before anything is published.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_state::external_execution::admission::AdmittedExternalCandidateProgram;
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_RELAY_CONTRACT, EXTERNAL_CHANNEL_ROUTE_CONTRACT,
    ExternalControllerFileRelayContract, ExternalControllerTransportContract,
    ExternalControllerTransportSelection, ExternalNetworkInputPolicy,
    ExternalNetworkInputSelection, MAX_TLS_ROOT_CERTIFICATES, external_tls_root_bundle_digest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    BindingDocument, ExternalExecutionSection, ExternalRuntimeQualificationBinding,
    ExternalStructuredSessionBinding, ExternalWorkloadBinding, InstalledExternalExecutionBinding,
    SECTION_NAME, adapter_settings_digest, binding_digest, capacity_owner,
};
use crate::node_config::NodeConfigSection as _;

/// Backend id the `render` shorthand selects.
pub const RENDER_BACKEND_ID: &str = "render-sandbox-early-access";
/// Capacity group an operator binding joins unless told otherwise.
pub const DEFAULT_CAPACITY_GROUP: &str = "candidate-workers";

const HTTP_NETWORK_POLICY: &str = "supervisor_pinned_owner_only_candidate_denied_v1";
const RELAY_NETWORK_POLICY: &str = "controller_file_relay_guest_deny_all_v1";
const STORAGE_POLICY: &str = "ephemeral_private_candidate_v1";
const CLEANUP_PROOF: &str = "provider_terminal_occurrence_v1";
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
    pub(crate) artifacts: BundleArtifactFacts,
    pub(crate) limits: ComposeLimits,
}

/// The single non-test constructor of a placement binding document. The
/// schema, network policy, storage policy and cleanup proof follow from the
/// transport; the digest of the settings is derived, never supplied.
pub(crate) fn compose_binding_document(inputs: ComposeInputs<'_>) -> Result<BindingDocument> {
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
        artifacts,
        limits,
    } = inputs;
    program.validate()?;
    controller_transport.validate()?;
    provider_network_inputs.validate()?;
    let (schema, network_policy) = match &controller_transport {
        ExternalControllerTransportSelection::HttpJson(http) => {
            ensure!(
                external_tls_root_bundle_digest(&tls_roots_der_base64)? == http.tls_root_bundle_digest,
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
        runtime_content_qualification: None,
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
    ensure!(pem.len() <= MAX_ROOTS_PEM_BYTES, "TLS roots PEM exceeds its byte bound");
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
                        .map_err(|_| anyhow::anyhow!("TLS root certificate is not an X.509 certificate"))?;
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
    ensure!(!certificates.is_empty(), "TLS roots PEM carries no CERTIFICATE block");
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
fn operator_network_inputs() -> ExternalNetworkInputPolicy {
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

struct SelectedTransport {
    selection: ExternalControllerTransportSelection,
    network_inputs: ExternalNetworkInputPolicy,
    tls_roots: Vec<String>,
}

fn select_transport(request: &ComposeTransportRequest, settings: &Value) -> Result<SelectedTransport> {
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
                settings.get("owner_id").and_then(Value::as_str) == Some(owner_workspace_id.as_str()),
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

fn validate_binding_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && !id.starts_with('.')
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')),
        "external-execution binding id is invalid"
    );
    Ok(())
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

/// Resolve the signed artifacts of a stopped node's installed bundles
/// without starting it. Holding the bundle-generation lock keeps the
/// installed set fixed for the whole read.
fn resolve_installed_artifacts(
    app_root: &Path,
) -> Result<crate::external_artifacts::ResolvedExternalExecutionArtifacts> {
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
        crate::external_artifacts::resolve_external_execution_artifacts(
            &generation.roots(),
            &trust,
            &crate::private_artifact_home::root(&roots.state()),
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
    pub settings: Value,
    pub guest_runtime_manifest_hash: String,
    pub runtime_qualification: Option<Value>,
    pub limits: ComposeLimits,
}

/// What was installed. It carries no credential and no vault key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    request.state_lock.ensure_protects_app_root(request.app_root)?;
    validate_binding_id(&request.id)?;
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(
        &request
            .app_root
            .join(ryeos_engine::AI_DIR)
            .join("node/identity/private_key.pem"),
    )
    .context("load node identity")?;
    let backend = canonical_backend_id(&request.backend).to_owned();
    let transport = select_transport(&request.transport, &request.settings)?;
    let runtime_qualification = request
        .runtime_qualification
        .map(|value| {
            serde_json::from_value::<ExternalRuntimeQualificationBinding>(value)
                .context("invalid runtime qualification")
        })
        .transpose()?;
    let resolved = resolve_installed_artifacts(request.app_root)?;
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
        credential_generation: fresh_credential_generation()?,
        settings: request.settings,
        guest_runtime_manifest_hash: request.guest_runtime_manifest_hash,
        runtime_qualification,
        artifacts,
        limits: request.limits,
    })?;
    let signed = lillux::signature::sign_content_at(
        &serde_yaml::to_string(&document)?,
        identity.signing_key(),
        "#",
        None,
        &lillux::time::iso8601_now(),
    );
    ensure!(
        signed.len() as u64 <= ryeos_node_runtime::node_document::MAX_ITEM_BYTES,
        "signed external-execution binding exceeds its byte bound"
    );
    let installed = admit_signed_binding(request.app_root, &request.id, &signed, &identity)?;
    let path = publish_signed_binding(request.app_root, &request.id, signed.as_bytes())?;
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

/// Verify signed bytes exactly as the loader would: a trusted signature
/// whose signer is the current node, then the section's own parse.
pub(crate) fn admit_signed_binding(
    app_root: &Path,
    id: &str,
    signed: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> Result<InstalledExternalExecutionBinding> {
    let envelope = ryeos_engine::contracts::SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let header = ryeos_engine::item_resolution::parse_signature_header(signed, &envelope)
        .context("composed binding has no valid signature line")?;
    ensure!(
        header.signer_fingerprint == identity.fingerprint(),
        "external-execution binding must be signed by the current node {}; got {}",
        identity.fingerprint(),
        header.signer_fingerprint
    );
    let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
        ryeos_engine::trust::TrustedSigner {
            fingerprint: identity.fingerprint().to_owned(),
            verifying_key: *identity.verifying_key(),
            label: None,
        },
    ]);
    let (class, _) = ryeos_engine::trust::verify_item_signature(signed, &header, &envelope, &trust)
        .context("verify composed binding signature")?;
    ensure!(
        class == ryeos_engine::contracts::TrustClass::Trusted,
        "composed binding signature is not trusted"
    );
    let body: Value = serde_yaml::from_str(
        &ryeos_node_runtime::node_config_source::strip_signature(signed),
    )
    .context("parse composed binding body")?;
    let rel_path = PathBuf::from(format!("{id}.yaml"));
    let context = ryeos_node_runtime::node_config_source::NodeItemContext {
        section: SECTION_NAME.into(),
        id: id.to_owned(),
        stem: id.to_owned(),
        source_file: app_root
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join(SECTION_NAME)
            .join(&rel_path),
        rel_path,
        signer_fingerprint: header.signer_fingerprint.clone(),
    };
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

fn publish_signed_binding(app_root: &Path, id: &str, bytes: &[u8]) -> Result<PathBuf> {
    let base = lillux::PinnedDirectory::open_or_create(
        &app_root.join(ryeos_engine::AI_DIR).join("node"),
    )
    .context("establish no-follow node document root")?;
    let section = base
        .open_or_create_child(std::ffi::OsStr::new(SECTION_NAME), 0o755)
        .context("establish external-execution section")?;
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
                "write external-execution binding {}",
                section.path().join(filename).display()
            )
        })?;
    Ok(section.path().join(filename))
}

#[cfg(test)]
mod tests {
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

    fn cert_a() -> String {
        pem_block("CERTIFICATE", CERT_A_BODY)
    }

    fn cert_b() -> String {
        pem_block("CERTIFICATE", CERT_B_BODY)
    }

    fn der_of(body: &str) -> Vec<u8> {
        STANDARD.decode(body.replace('\n', "")).unwrap()
    }

    fn root_pem() -> String {
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

    #[test]
    fn both_transports_validate_and_pick_their_schema_and_network_policy() {
        let program = program();
        let http = compose(&program, &https(), serde_json::json!({"region": "r"})).unwrap();
        assert_eq!((http.schema, http.network_policy.as_str()), (17, HTTP_NETWORK_POLICY));
        assert_eq!(http.controller_tls_root_certificates_der_base64.len(), 2);
        let file = compose(&program, &relay(), relay_settings()).unwrap();
        assert_eq!((file.schema, file.network_policy.as_str()), (19, RELAY_NETWORK_POLICY));
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
        assert_eq!(document.settings_schema_digest, facts.settings_schema_digest);
        assert_eq!(document.supervisor_artifact_hash, facts.supervisor_artifact_hash);
        assert_eq!(document.launcher_artifact_hash, facts.launcher_artifact_hash);
        let session = document.workload.structured_session().unwrap();
        assert_eq!(session.configuration_adapter_artifact_hash, facts.configuration_adapter_artifact_hash);
        assert_eq!(session.connector_artifact_hash, facts.connector_artifact_hash);
        assert_eq!(session.runtime_manifest_hash, program.runtime_manifest_hash);
        assert_eq!(session.runtime_selection_identity, program.selection_identity_digest);
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
                error.to_string().contains("without path, query, fragment or userinfo"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn ip_literal_origins_are_accepted_in_canonical_form() {
        for good in ["https://192.0.2.1:7443", "https://[::1]:7443"] {
            assert_eq!(canonical_https_origin(good).unwrap(), good);
        }
        for bad in ["https://[::1]:443", "https://[0:0:0:0:0:0:0:1]:7443", "https://192.0.2.1:0"] {
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
            settings: serde_json::json!({}),
            guest_runtime_manifest_hash: "4".repeat(64),
            runtime_qualification: None,
            limits: ComposeLimits::default(),
        }));
        assert!(!format!("{error:#}").is_empty());
        assert!(!root.path().join(".ai/node/external_execution/operator.yaml").exists());
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
            format!("{}-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", root_pem()),
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
        assert!(error.to_string().contains("not an X.509 certificate"), "{error}");
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
        let error = refusal(compose(&program, &relay(), serde_json::json!({"owner_id": "other"})));
        assert!(error.to_string().contains("owner_id must equal the owner workspace id"), "{error}");
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
            artifacts: facts(),
            limits: ComposeLimits::default(),
        }));
        assert!(error.to_string().contains("carries no TLS roots"), "{error}");
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
        let error = admit_signed_binding(root.path(), "operator", &sign(&foreign), &identity).unwrap_err();
        assert!(error.to_string().contains("must be signed by the current node"), "{error}");
    }

    #[test]
    fn published_file_is_replaced_atomically_and_carries_the_signed_bytes() {
        let root = tempfile::tempdir().unwrap();
        let first = publish_signed_binding(root.path(), "operator", b"first").unwrap();
        assert_eq!(first, root.path().join(".ai/node/external_execution/operator.yaml"));
        let second = publish_signed_binding(root.path(), "operator", b"second").unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
    }

    #[test]
    fn binding_id_is_a_bounded_identifier() {
        assert!(validate_binding_id("render-1.a_b").is_ok());
        for bad in ["", ".hidden", "a/b", "a b", &"a".repeat(129)] {
            assert!(validate_binding_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn limits_file_overrides_named_fields_and_refuses_unknown_ones() {
        let limits: ComposeLimits = serde_json::from_value(serde_json::json!({"max_active": 3})).unwrap();
        assert_eq!(limits.max_active, 3);
        assert_eq!(limits.timeout_seconds, ComposeLimits::default().timeout_seconds);
        assert!(serde_json::from_value::<ComposeLimits>(serde_json::json!({"max_actve": 3})).is_err());
    }
}
