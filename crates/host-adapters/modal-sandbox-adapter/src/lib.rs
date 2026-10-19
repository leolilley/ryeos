//! Fail-closed controller-side adapter that runs RyeOS leases as Modal
//! sandboxes through the signed lease facade. Modal's own client protocol is
//! not HTTP/1.1, so every provider contact is a JSON POST to one of the
//! facade's fixed routes; the signed provider spec pins the facade source
//! digest every reply must carry.
//!
//! Guest activation is delivered only when the signed spec declares the
//! staged sequence; it always settles pending, and the guest's own
//! authenticated channel alone can establish Ready.

pub mod activation;
pub mod facade;
pub mod lease_ops;
pub mod provider_spec;
pub mod transport;

use std::collections::{BTreeMap, BTreeSet};
use std::thread::JoinHandle;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use lillux::network::{NetworkCancellation, NetworkContext};
use ryeos_external_execution_contract::{
    LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV, LIFECYCLE_ADAPTER_PROTOCOL,
    LIFECYCLE_GUEST_RUNTIME_FD_ENV, LIFECYCLE_HOSTS_FD_ENV, LIFECYCLE_HOSTS_SHA256_ENV,
    LIFECYCLE_NETWORK_POLICY_SHA256_ENV, LIFECYCLE_PROVIDER_SPEC_FD_ENV,
    LIFECYCLE_PROVIDER_SPEC_SHA256_ENV, LIFECYCLE_REQUEST_FD_ENV, LIFECYCLE_RESOLVER_FD_ENV,
    LIFECYCLE_RESOLVER_SHA256_ENV, LIFECYCLE_SETTINGS_FD_ENV, LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
    LIFECYCLE_SIGNED_IMPORT_FD_ENV, LifecycleAdapterInspectionRequest,
    LifecycleAdapterInspectionResponse, LifecycleAdapterRequest, LifecycleAdapterResponse,
    LifecycleArtifactInspection, LifecycleArtifactRole, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES,
    MAX_LIFECYCLE_REQUEST_BYTES, canonical_json, from_json_slice_strict,
};
use ryeos_remote_lease_core::{
    operation_deadline, read_credential, read_sealed_env, request_deadline, write_response,
};
use serde::Deserialize;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use facade::FacadeClient;
use lease_ops::LeaseContext;
use provider_spec::ProviderSpec;
use transport::{HttpFacadeTransport, ProxyToken, facade_host};

const ADAPTER_ID: &str = "modal-sandbox-lease";
const ADAPTER_BUILD: &str = "ryeos-modal-sandbox-lifecycle-adapter.0.1.0";
const SETTINGS_SCHEMA: &[u8] = include_bytes!("../fixtures/settings.schema.json");
const MAX_SETTINGS_BYTES: usize = 256 * 1024;
const MAX_ROOTS: usize = 8;
const MAX_ROOT_TOTAL_BYTES: usize = 256 * 1024;
const MAX_ROOT_BYTES: usize = 64 * 1024;
const MAX_NETWORK_INPUT_BYTES: usize = 64 * 1024;
/// How long a concurrent create is given to finish tagging its lease.
const UNTAGGED_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    schema: u32,
    workspace: String,
    lease_set: String,
    tls_roots_der_base64: Vec<String>,
}

pub fn run() -> Result<()> {
    // SAFETY: the trusted lifecycle runner installs this descriptor exactly
    // once before this single-threaded startup boundary.
    let adapter = unsafe {
        lillux::take_inherited_descriptor_authority_from_env(LIFECYCLE_ADAPTER_EXECUTABLE_FD_ENV)
    }
    .map_err(anyhow::Error::msg)?;
    match std::env::args().nth(1).as_deref() {
        Some("inspect") => inspect(&adapter),
        Some("operate") => operate(),
        Some("admit-lease") => ryeos_remote_lease_core::admit_lease(ADAPTER_ID, parse_lease_spec),
        Some("await-lease-ready") => {
            ryeos_remote_lease_core::await_lease_ready(ADAPTER_ID, parse_lease_spec)
        }
        _ => anyhow::bail!("unsupported lifecycle invocation"),
    }
}

fn settings_schema_digest() -> String {
    lillux::sha256_hex(SETTINGS_SCHEMA)
}

fn parse_provider_spec_bytes(bytes: &[u8], declared_digest: &str) -> Result<ProviderSpec> {
    let maximum = usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= maximum,
        "provider spec is empty or exceeds its byte bound"
    );
    ensure!(
        lillux::valid_hash(declared_digest) && lillux::sha256_hex(bytes) == declared_digest,
        "provider spec differs from its signed digest"
    );
    ProviderSpec::parse(bytes, &settings_schema_digest())
}

fn parse_lease_spec(provider_spec_bytes: &[u8], captured_sha256: &str) -> Result<ProviderSpec> {
    parse_provider_spec_bytes(provider_spec_bytes, captured_sha256)
}

fn verify_artifact(
    authority: &lillux::InheritedDescriptorAuthority,
    expected_digest: &str,
    expected_bytes: Option<u64>,
) -> Result<LifecycleArtifactInspection> {
    authority.require_owned_executable()?;
    let observation = authority.regular_file_observation()?;
    ensure!(
        expected_bytes.is_none_or(|bytes| bytes == observation.size())
            && authority.digest_regular_file_stable_exact(&observation)? == expected_digest,
        "lifecycle artifact identity changed"
    );
    Ok(LifecycleArtifactInspection {
        descriptor: authority
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        digest: expected_digest.into(),
        bytes: observation.size(),
    })
}

fn inspect(adapter: &lillux::InheritedDescriptorAuthority) -> Result<()> {
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LIFECYCLE_REQUEST_BYTES)?;
    let request: LifecycleAdapterInspectionRequest =
        from_json_slice_strict(&request_bytes, MAX_LIFECYCLE_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        canonical_json(&request)? == request_bytes,
        "inspection request is noncanonical"
    );
    let schema_digest = settings_schema_digest();
    ensure!(
        request.adapter_id == ADAPTER_ID
            && request.protocol == LIFECYCLE_ADAPTER_PROTOCOL
            && request.target == lillux::platform::current_binary_target()?
            && request.settings_schema_digest == schema_digest
            && request.snapshot_production_spec.is_none(),
        "inspection does not match this adapter's narrow declaration"
    );
    // SAFETY: the trusted runner transferred this exact descriptor once into
    // this single-threaded inspection process.
    let provider_spec_authority =
        unsafe { lillux::take_inherited_descriptor_authority(request.provider_spec.descriptor) }
            .map_err(anyhow::Error::msg)?;
    let provider_spec_bytes = lillux::read_sealed_inherited_descriptor(
        provider_spec_authority
            .inherited_descriptor()
            .map_err(anyhow::Error::msg)?,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )
    .map_err(anyhow::Error::msg)?;
    ensure!(
        u64::try_from(provider_spec_bytes.len())? == request.provider_spec.bytes,
        "provider spec size differs from the inspection declaration"
    );
    let provider_spec =
        parse_provider_spec_bytes(&provider_spec_bytes, &request.provider_spec.digest)?;
    let effective_capabilities = provider_spec.effective_capabilities();
    ensure!(
        !effective_capabilities.is_empty()
            && effective_capabilities.is_subset(&request.declared_capabilities),
        "provider spec proof profiles exceed the signed capability ceiling"
    );
    verify_artifact(adapter, &request.adapter_artifact_hash, None)?;
    let mut artifacts = BTreeMap::new();
    for (role, inspection) in &request.artifacts {
        // SAFETY: the trusted runner transferred each exact artifact
        // descriptor once for this single-threaded inspection process.
        let authority =
            unsafe { lillux::take_inherited_descriptor_authority(inspection.descriptor) }
                .map_err(anyhow::Error::msg)?;
        artifacts.insert(
            *role,
            verify_artifact(&authority, &inspection.digest, Some(inspection.bytes))?,
        );
    }
    ensure!(
        (2..=3).contains(&artifacts.len())
            && artifacts.contains_key(&LifecycleArtifactRole::Supervisor)
            && artifacts.contains_key(&LifecycleArtifactRole::Launcher),
        "inspection requires exact bootstrap artifacts"
    );
    let response = LifecycleAdapterInspectionResponse {
        schema: 1,
        protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
        adapter_id: ADAPTER_ID.into(),
        adapter_build: ADAPTER_BUILD.into(),
        observed_adapter_artifact_hash: request.adapter_artifact_hash.clone(),
        observed_settings_schema_digest: schema_digest,
        target: request.target.clone(),
        effective_capabilities,
        observed_provider_spec_sha256: request.provider_spec.digest.clone(),
        observed_snapshot_production_spec_sha256: None,
        observed_bootstrap_profile_digest: None,
        artifacts,
    };
    response.validate_for(&request)?;
    write_response(&response)
}

fn operate() -> Result<()> {
    // The one operation deadline starts before any descriptor is read; local
    // work never renews it.
    let mut deadline = operation_deadline()?;
    let cancellation = NetworkCancellation::default();
    let _signal_cancellation = SignalCancellation::install(cancellation.clone())?;
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LIFECYCLE_REQUEST_BYTES)?;
    let request: LifecycleAdapterRequest =
        from_json_slice_strict(&request_bytes, MAX_LIFECYCLE_REQUEST_BYTES)?;
    request.validate()?;
    ensure!(
        request.canonical_bytes()? == request_bytes,
        "operation request is noncanonical"
    );
    let provider_spec_bytes = read_sealed_env(
        LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let provider_spec_sha256 = std::env::var(LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("missing captured provider spec digest")?;
    let provider_spec = parse_provider_spec_bytes(&provider_spec_bytes, &provider_spec_sha256)?;
    // A lease's admitted request names its guest egress; it selects the exact
    // policy the create requests. A policy the signed spec cannot create
    // fails closed before any credential or network input is read.
    let lease_egress = match ryeos_remote_lease_core::read_lease_admission()? {
        Some(admission) => {
            ensure!(
                admission.adapter_id == ADAPTER_ID
                    && admission.provider_spec_sha256 == provider_spec_sha256,
                "lease admission names a different adapter or provider spec"
            );
            admission.guest_network_allow
        }
        None => BTreeSet::new(),
    };
    let egress_policy = provider_spec.egress_policy(&lease_egress)?;
    // Reconciling an activation never contacts the provider: it settles
    // pending before any guest input, credential or network input is read.
    if let Some(response) = reconcile_activation_response(&request) {
        response.validate_for(&request)?;
        return write_response(&response);
    }
    // A spec that does not declare the staged sequence refuses a claimed
    // activation before any guest input, credential or network input is read.
    let prepared = if let LifecycleAdapterRequest::ActivateSupervisor {
        common: _,
        occurrence: _,
        activation,
        guest_input_identity: _,
        guest_input_projection: _,
        guest_package: _,
        import_ticket: _,
    } = &request
    {
        require_declared_activation(&provider_spec)?;
        provider_spec.require_guest_runtime_descriptor(
            std::env::var_os(LIFECYCLE_GUEST_RUNTIME_FD_ENV).is_some(),
        )?;
        deadline = request_deadline(activation.attachment_deadline_ms, deadline)?;
        // SAFETY: the trusted lifecycle runner installed each of these
        // descriptors exactly once for this operation.
        Some(unsafe { activation::PreparedActivation::from_environment(&request) }?)
    } else {
        ensure!(
            std::env::var_os(LIFECYCLE_SIGNED_IMPORT_FD_ENV).is_none()
                && std::env::var_os(LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV).is_none()
                && std::env::var_os(LIFECYCLE_GUEST_RUNTIME_FD_ENV).is_none(),
            "signed guest import, assignment or runtime was supplied outside first activation"
        );
        None
    };
    if let LifecycleAdapterRequest::Allocate { reservation, .. } = &request {
        deadline = request_deadline(reservation.contact_deadline_ms, deadline)?;
    }
    if matches!(
        &request,
        LifecycleAdapterRequest::ReconcileAllocation { .. }
    ) {
        ensure!(
            provider_spec.reconciles_allocation(),
            "signed Modal provider spec does not declare allocation reconciliation"
        );
    }
    let settings_bytes = read_sealed_env(LIFECYCLE_SETTINGS_FD_ENV, MAX_SETTINGS_BYTES)?;
    ensure!(
        lillux::sha256_hex(&settings_bytes) == request.common().settings_digest,
        "settings digest changed"
    );
    let settings_value: serde_json::Value =
        from_json_slice_strict(&settings_bytes, MAX_SETTINGS_BYTES)?;
    ensure!(
        canonical_json(&settings_value)? == settings_bytes,
        "settings are noncanonical"
    );
    let settings: Settings = serde_json::from_value(settings_value)?;
    validate_settings(&settings)?;
    let network = network_context_from_captured_inputs()?;
    let token = ProxyToken::parse(&read_credential("Modal")?)?;
    let facade_transport = HttpFacadeTransport {
        network: &network,
        host: facade_host(&settings.workspace, &provider_spec.facade().endpoint_label),
        token,
        tls_roots_der: tls_roots_from_base64(&settings.tls_roots_der_base64)?,
        deadline,
        cancellation: &cancellation,
    };
    let client = FacadeClient::new(&facade_transport, provider_spec.facade());
    let context = LeaseContext {
        lease_set: &settings.lease_set,
        untagged_settle: UNTAGGED_SETTLE,
    };
    let response = match &request {
        LifecycleAdapterRequest::Allocate {
            common,
            reservation,
        } => lease_ops::allocate(&client, &context, common, reservation, &egress_policy),
        LifecycleAdapterRequest::ReconcileAllocation {
            common,
            reservation,
        } => {
            lease_ops::reconcile_allocation(&client, &context, common, reservation, &egress_policy)
        }
        LifecycleAdapterRequest::Terminate {
            common,
            occurrence,
            termination,
        } => lease_ops::terminate(&client, common, occurrence, termination),
        LifecycleAdapterRequest::ReconcileTermination {
            common,
            occurrence,
            termination,
        } => lease_ops::observe_termination(&client, common, occurrence, termination),
        LifecycleAdapterRequest::ActivateSupervisor {
            common,
            occurrence,
            activation,
            guest_input_identity: _,
            guest_input_projection: _,
            guest_package: _,
            import_ticket: _,
        } => {
            let prepared = prepared
                .as_ref()
                .context("first activation lost its verified sealed inputs")?;
            // Once this call begins any step may have reached the facade, and
            // even a complete delivery is not an authenticated Ready: every
            // outcome is pending for the guest's own channel to settle.
            activation::pending_unless_refused(
                prepared.contact(&client, &occurrence.occurrence_id),
            )?;
            LifecycleAdapterResponse::SupervisorPending {
                operation_id: common.operation_id.clone(),
                activation_request_digest: activation.activation_request_digest.clone(),
            }
        }
        LifecycleAdapterRequest::ReconcileSupervisorActivation { .. } => {
            anyhow::bail!("activation reconciliation settles before provider contact")
        }
    };
    response.validate_for(&request)?;
    write_response(&response)
}

/// Reconciling an activation is observation only: pending, no contact.
fn reconcile_activation_response(
    request: &LifecycleAdapterRequest,
) -> Option<LifecycleAdapterResponse> {
    match request {
        LifecycleAdapterRequest::ReconcileSupervisorActivation {
            common, activation, ..
        } => Some(LifecycleAdapterResponse::SupervisorPending {
            operation_id: common.operation_id.clone(),
            activation_request_digest: activation.activation_request_digest.clone(),
        }),
        _ => None,
    }
}

fn require_declared_activation(spec: &ProviderSpec) -> Result<()> {
    ensure!(
        spec.declares_activation(),
        "signed Modal provider spec does not declare supervisor activation"
    );
    Ok(())
}

fn validate_settings(settings: &Settings) -> Result<()> {
    ensure!(settings.schema == 1, "unsupported settings schema");
    ensure!(
        valid_workspace(&settings.workspace),
        "invalid Modal workspace"
    );
    ensure!(valid_lease_set(&settings.lease_set), "invalid lease set");
    validate_tls_roots(&settings.tls_roots_der_base64)
}

/// A workspace name as one DNS label part of the facade host: lowercase
/// alphanumerics with single interior hyphens, short enough that the label
/// `<workspace>--<endpoint label>` stays within 63 bytes.
fn valid_workspace(workspace: &str) -> bool {
    let bytes = workspace.as_bytes();
    (1..=40).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && !workspace.starts_with('-')
        && !workspace.ends_with('-')
        && !workspace.contains("--")
}

fn valid_lease_set(lease_set: &str) -> bool {
    let bytes = lease_set.as_bytes();
    (1..=63).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn validate_tls_roots(roots: &[String]) -> Result<()> {
    ensure!(
        !roots.is_empty() && roots.len() <= MAX_ROOTS,
        "explicit facade TLS roots are missing or excessive"
    );
    let mut total = 0usize;
    let mut previous: Option<&str> = None;
    for root in roots {
        ensure!(
            previous.is_none_or(|value| value < root.as_str()),
            "TLS roots are not uniquely ordered"
        );
        previous = Some(root);
        ensure!(
            root.len() <= MAX_ROOT_BYTES.div_ceil(3) * 4,
            "TLS root exceeds its encoded bound"
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(root)
            .context("invalid TLS root encoding")?;
        ensure!(
            !bytes.is_empty()
                && bytes.len() <= MAX_ROOT_BYTES
                && base64::engine::general_purpose::STANDARD.encode(&bytes) == *root,
            "TLS root encoding or size is invalid"
        );
        total = total
            .checked_add(bytes.len())
            .context("TLS root byte count overflow")?;
    }
    ensure!(
        total <= MAX_ROOT_TOTAL_BYTES,
        "TLS root bundle exceeds its byte bound"
    );
    Ok(())
}

fn tls_roots_from_base64(roots: &[String]) -> Result<Vec<Vec<u8>>> {
    roots
        .iter()
        .map(|root| {
            base64::engine::general_purpose::STANDARD
                .decode(root)
                .context("invalid TLS root encoding")
        })
        .collect()
}

fn network_context_from_captured_inputs() -> Result<NetworkContext> {
    let resolver = read_sealed_env(LIFECYCLE_RESOLVER_FD_ENV, MAX_NETWORK_INPUT_BYTES)?;
    let hosts = read_sealed_env(LIFECYCLE_HOSTS_FD_ENV, MAX_NETWORK_INPUT_BYTES)?;
    let resolver_digest =
        std::env::var(LIFECYCLE_RESOLVER_SHA256_ENV).context("missing captured resolver digest")?;
    let hosts_digest =
        std::env::var(LIFECYCLE_HOSTS_SHA256_ENV).context("missing captured hosts digest")?;
    let network_policy_digest = std::env::var(LIFECYCLE_NETWORK_POLICY_SHA256_ENV)
        .context("missing captured network policy digest")?;
    ensure!(
        lillux::valid_hash(&resolver_digest)
            && lillux::valid_hash(&hosts_digest)
            && lillux::valid_hash(&network_policy_digest)
            && lillux::sha256_hex(&resolver) == resolver_digest
            && lillux::sha256_hex(&hosts) == hosts_digest,
        "captured network input identity changed"
    );
    NetworkContext::from_config_bytes(&resolver, &hosts).context("invalid captured network inputs")
}

struct SignalCancellation {
    handle: SignalHandle,
    listener: Option<JoinHandle<()>>,
}

impl SignalCancellation {
    fn install(cancellation: NetworkCancellation) -> Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM])?;
        let handle = signals.handle();
        let listener = std::thread::Builder::new()
            .name("modal-adapter-cancel".into())
            .spawn(move || {
                if signals.forever().next().is_some() {
                    cancellation.cancel();
                }
            })?;
        Ok(Self {
            handle,
            listener: Some(listener),
        })
    }
}

impl Drop for SignalCancellation {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(count: usize) -> Vec<String> {
        let mut roots: Vec<String> = (0..count)
            .map(|index| base64::engine::general_purpose::STANDARD.encode([index as u8 + 1; 4]))
            .collect();
        roots.sort();
        roots
    }

    fn settings() -> Settings {
        Settings {
            schema: 1,
            workspace: "acme-prod".into(),
            lease_set: "node-a".into(),
            tls_roots_der_base64: roots(1),
        }
    }

    #[test]
    fn settings_validate_workspace_lease_set_and_roots() {
        validate_settings(&settings()).unwrap();
        for bad in ["", "-a", "a-", "a--b", "A", "a_b", &"a".repeat(41)] {
            let mut value = settings();
            value.workspace = bad.into();
            assert!(validate_settings(&value).is_err(), "{bad:?}");
        }
        for bad in ["", "-a", "A", "a_b", &"a".repeat(64)] {
            let mut value = settings();
            value.lease_set = bad.into();
            assert!(validate_settings(&value).is_err(), "{bad:?}");
        }
        let mut value = settings();
        value.schema = 2;
        assert!(validate_settings(&value).is_err());
        let mut value = settings();
        value.tls_roots_der_base64 = Vec::new();
        assert!(validate_settings(&value).is_err());
        let mut value = settings();
        value.tls_roots_der_base64 = roots(9);
        assert!(validate_settings(&value).is_err());
        let mut value = settings();
        value.tls_roots_der_base64 = vec!["not base64!".into()];
        assert!(validate_settings(&value).is_err());
        let mut value = settings();
        let mut descending = roots(2);
        descending.reverse();
        value.tls_roots_der_base64 = descending;
        assert!(validate_settings(&value).is_err());
    }

    fn declared_spec_bytes() -> Vec<u8> {
        let mut value: serde_json::Value =
            serde_json::from_slice(include_bytes!("../fixtures/provider-spec.json")).unwrap();
        value["operations"]["activate_supervisor"] = serde_json::json!({
            "kind": "stage_then_upload_then_run_once",
            "stage_route": "stage",
            "seal_route": "stage_seal",
            "upload_route": "upload",
            "run_route": "run_once",
        });
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn an_undeclared_spec_refuses_activation_and_a_declared_one_allows_it() {
        let undeclared = include_bytes!("../fixtures/provider-spec.json");
        let spec = parse_lease_spec(undeclared, &lillux::sha256_hex(undeclared)).unwrap();
        assert!(require_declared_activation(&spec).is_err());
        let declared = declared_spec_bytes();
        let spec = parse_lease_spec(&declared, &lillux::sha256_hex(&declared)).unwrap();
        require_declared_activation(&spec).unwrap();
    }

    #[test]
    fn reconciling_an_activation_settles_pending_without_contact() {
        use ryeos_external_execution_contract::{
            BoundOccurrence, LifecycleOperationCommon, SupervisorActivationIntent,
        };
        let request = LifecycleAdapterRequest::ReconcileSupervisorActivation {
            common: LifecycleOperationCommon {
                schema: 1,
                protocol: LIFECYCLE_ADAPTER_PROTOCOL.into(),
                operation_id: "op".into(),
                binding_hash: "1".repeat(64),
                settings_digest: "2".repeat(64),
            },
            occurrence: BoundOccurrence {
                request_digest: "3".repeat(64),
                occurrence_id: "sb-1".into(),
            },
            activation: SupervisorActivationIntent {
                activation_request_digest: "4".repeat(64),
                supervisor_runtime_hash: "5".repeat(64),
                launcher_artifact_hash: "6".repeat(64),
                guest_runtime_artifact_hash: Some("7".repeat(64)),
                guest_runtime_artifact_bytes: Some(1),
                attachment_deadline_ms: 1,
                execution_timeout_seconds: 1,
                post_execution_timeout_seconds: 1,
                channel_max_bytes: 1,
            },
        };
        assert!(matches!(
            reconcile_activation_response(&request),
            Some(LifecycleAdapterResponse::SupervisorPending { .. })
        ));
    }

    #[test]
    fn settings_reject_unknown_fields() {
        let value = serde_json::json!({
            "schema": 1, "workspace": "acme", "lease_set": "a",
            "tls_roots_der_base64": [], "extra": 1,
        });
        assert!(serde_json::from_value::<Settings>(value).is_err());
    }

    #[test]
    fn the_settings_schema_digest_is_the_one_the_spec_signs() {
        let spec: serde_json::Value =
            serde_json::from_slice(include_bytes!("../fixtures/provider-spec.json")).unwrap();
        assert_eq!(spec["settings_schema_digest"], settings_schema_digest());
        let bundle = include_bytes!("../../../../bundles/modal-sandbox/specs/settings.schema.json");
        assert_eq!(bundle.as_slice(), SETTINGS_SCHEMA);
    }

    #[test]
    fn the_manifest_pins_the_fixture_spec_and_schema_digests() {
        let manifest = include_str!("../../../../bundles/modal-sandbox/.ai/manifest.source.yaml");
        let spec = include_bytes!("../fixtures/provider-spec.json");
        assert!(manifest.contains(&format!("sha256: \"{}\"", lillux::sha256_hex(spec))));
        assert!(manifest.contains(&format!(
            "settings_schema_digest: \"{}\"",
            settings_schema_digest()
        )));
        assert!(manifest.contains(&format!("id: {ADAPTER_ID}")));
    }

    #[test]
    fn provider_spec_digest_is_checked_before_parsing() {
        let spec = include_bytes!("../fixtures/provider-spec.json");
        let digest = lillux::sha256_hex(spec);
        parse_lease_spec(spec, &digest).unwrap();
        assert!(parse_lease_spec(spec, &"0".repeat(64)).is_err());
        assert!(parse_lease_spec(spec, "short").is_err());
        assert!(parse_lease_spec(b"", &digest).is_err());
    }
}
