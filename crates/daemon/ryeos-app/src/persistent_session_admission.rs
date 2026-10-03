//! Current verification of exact retained persistent-session bytes.
//!
//! These shared checks are not a complete admitted session owner, launch
//! permit, resource reservation or cleanup proof. Mechanical plan recovery
//! still cannot mint trusted authority. Original exact-program, attachment,
//! realization verification remains read-only; custody and executable admission
//! remain with their existing owners.

use anyhow::{Context as _, Result, anyhow, bail};
use ryeos_engine::protocols::validate_persistent_session_protocol;
use ryeos_state::objects::AdmittedPersistentSessionCapsule;
use serde::{Deserialize, Serialize};

use crate::prepared_evidence_attachment::PreparedEvidenceAttachment;

pub(crate) mod cleanup_channel;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionExactProgram {
    pub effective_definition_digest: String,
    pub resolution_output: ryeos_engine::resolution::RetainedResolutionOutput,
    pub evidence_attachments: Vec<PreparedEvidenceAttachment>,
}

pub fn validate_exact_evidence_attachments(exact: &PersistentSessionExactProgram) -> Result<()> {
    let mut previous: Option<&str> = None;
    for binding in &exact.evidence_attachments {
        binding.validate()?;
        if previous.is_some_and(|value| value >= binding.binding_id.as_str()) {
            bail!("persistent-session evidence attachments are not canonically ordered");
        }
        previous = Some(&binding.binding_id);
    }
    Ok(())
}

pub fn retained_exact_program(
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<PersistentSessionExactProgram> {
    serde_json::from_value(capsule.retained_exact_program()?)
        .context("decode full retained persistent-session exact program")
}

pub fn retained_session_protocol(
    engine: &ryeos_engine::engine::Engine,
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<ryeos_engine::protocols::descriptor::PersistentSessionProtocol> {
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        protocol_ref,
        protocol_content_hash,
        protocol_signer_fingerprint,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("persistent-session protocol has no direct artifact identity");
    };
    let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
        protocol_descriptor_document,
        execution_plan,
        ..
    } = &capsule.execution_closure
    else {
        bail!("persistent-session protocol has no retained direct closure");
    };
    // The current trust store may revoke the retained signer. It must never
    // replace admitted behavior with a newer descriptor at the same ref.
    let descriptor = verify_and_decode_retained_protocol_document(
        protocol_descriptor_document,
        protocol_content_hash,
        protocol_signer_fingerprint,
        &engine.node_trust_store,
    )
    .map_err(|error| anyhow!("verify retained session protocol: {error}"))?;
    ryeos_engine::protocols::validate_admitted_protocol_descriptor(protocol_ref, &descriptor)?;
    let session =
        validate_persistent_session_protocol(&descriptor).map_err(|error| anyhow!(error))?;
    let plan: ryeos_engine::contracts::ExecutionPlan =
        serde_json::from_value(execution_plan.clone())?;
    if plan
        .filesystem_authority_ceiling
        .intersect(session.workspace_authority.filesystem_ceiling())
        != plan.filesystem_authority_ceiling
        || plan
            .network_authority_ceiling
            .intersect(session.network_authority.network_ceiling())
            != plan.network_authority_ceiling
    {
        bail!("persistent-session plan widens its retained protocol ceilings");
    }
    Ok(session.clone())
}

fn verify_and_decode_retained_protocol_document(
    document: &str,
    expected_content_hash: &str,
    expected_signer: &str,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<ryeos_engine::protocols::ProtocolDescriptor> {
    let body = crate::admitted_program_trust::verify_admitted_signed_descriptor_document(
        document,
        expected_content_hash,
        expected_signer,
        trust_store,
    )?;
    decode_retained_protocol_descriptor(&body)
}

/// Decode already-admitted predecessor protocol bytes without weakening
/// current authoring. Before cleanup authority became explicit, pooled
/// sessions had no dedicated cleanup owner and exclusive sessions always
/// required the local Lillux process scope. The signed original bytes are
/// verified before this narrow historical interpretation is applied.
fn decode_retained_protocol_descriptor(
    body: &str,
) -> Result<ryeos_engine::protocols::ProtocolDescriptor> {
    let mut value: serde_json::Value = serde_yaml::from_str(body)?;
    if let Some(session) = value
        .get_mut("session")
        .and_then(serde_json::Value::as_object_mut)
        && !session.contains_key("cleanup_authority")
    {
        let historical = match session
            .get("process_mode")
            .and_then(serde_json::Value::as_str)
        {
            Some("pooled_requests") => "not_required",
            Some("exclusive_session") => "local_process_scope",
            _ => bail!("retained session protocol has no interpretable cleanup authority"),
        };
        session.insert(
            "cleanup_authority".to_owned(),
            serde_json::Value::String(historical.to_owned()),
        );
    }
    serde_json::from_value(value).context("decode retained session protocol descriptor")
}

pub fn validate_capsule_current_trust(
    engine: &ryeos_engine::engine::Engine,
    capsule: &AdmittedPersistentSessionCapsule,
) -> Result<()> {
    retained_session_protocol(engine, capsule)?;
    let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        root_subject_signer_fingerprint,
        root_subject_source_identity,
        protocol_signer_fingerprint,
        executable_identity,
        runtime_identity,
        ..
    } = &capsule.artifact_identity
    else {
        bail!("persistent-session capsule has a non-direct artifact identity");
    };
    let mut signers = Vec::new();
    if let Some(signer) = root_subject_signer_fingerprint.as_deref() {
        signers.push(("root subject", signer));
    }
    signers.push(("protocol", protocol_signer_fingerprint.as_str()));
    signers.push((
        "runtime",
        runtime_identity.runtime_signer_fingerprint.as_str(),
    ));
    if let Some(signer) = runtime_identity
        .runtime_bundle_signer_fingerprint
        .as_deref()
    {
        signers.push(("runtime bundle", signer));
    }
    if let ryeos_state::objects::DirectRootSourceIdentity::Bundle {
        manifest_signer_fingerprint,
        ..
    } = root_subject_source_identity
    {
        signers.push(("root bundle", manifest_signer_fingerprint));
    }
    if let ryeos_state::objects::DirectExecutableIdentity::BundleExecutor {
        executor_manifest_signer_fingerprint,
        ..
    } = executable_identity
    {
        signers.push(("executable bundle", executor_manifest_signer_fingerprint));
    }
    for (label, signer) in signers {
        if !engine.node_trust_store.is_trusted(signer) {
            bail!("persistent-session {label} signer is no longer trusted: {signer}");
        }
    }
    Ok(())
}

/// Exact read-only retained inputs for a local resource-bearing trusted pool.
/// This is NOT executable admission, a spawn permit, financial prebinding or
/// driver retirement. Private fields and no Clone/serde constructor prevent a
/// raw capsule, selected-resource DTO or caller flag from creating this proof.
pub struct VerifiedTrustedPooledSessionInputs {
    state_store: std::sync::Arc<crate::state_store::StateStore>,
    state_authority: ryeos_state::PinnedStateAuthority,
    capsule: AdmittedPersistentSessionCapsule,
    exact: PersistentSessionExactProgram,
    selected: crate::execution_resources::SelectedExecutionResources,
    cleanup_contract_digest: String,
    original_isolation: Option<OriginalIsolationExpectation>,
    original_product_inputs: Option<OriginalProductInputs>,
    original_startup_contract: Option<OriginalStartupContract>,
    startup_contract_selection:
        Option<ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection>,
}

// Static source custody, not a startup factory/accepted observation. Retain raw
// actual source/member owners before parsing and all later fallible checks.
struct OriginalStartupContract {
    selection: ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection,
    source: crate::source_closure_admission::AdmittedSourceClosure,
    member: Option<crate::source_closure_admission::LoadedStartupContractMember>,
    realizations: Option<ryeos_engine::external_realization::RealizedExternalContentSet>,
    contract: Option<StartupContract>,
    verified: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupSourceMember {
    root: String,
    path: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupContract {
    schema: String,
    role: String,
    entry: StartupSourceMember,
    bootstrap_members: Vec<StartupSourceMember>,
    launch: StartupLaunch,
    products: StartupProducts,
    policy: StartupPolicy,
    wire: StartupWire,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupLaunch {
    command_sha256: String,
    args: Vec<ryeos_engine::contracts::PlanArgument>,
    #[serde(deserialize_with = "deserialize_startup_environment")]
    env: std::collections::BTreeMap<String, String>,
    process_environment_sha256: String,
    dynamic_channels: Vec<StartupDynamicChannel>,
}

fn deserialize_startup_environment<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<std::collections::BTreeMap<String, String>, D::Error> {
    struct Environment;
    impl<'de> serde::de::Visitor<'de> for Environment {
        type Value = std::collections::BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a closed bounded startup environment without duplicate names")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut result = std::collections::BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if result.len() >= 32
                    || key.is_empty()
                    || key.len() > 128
                    || value.len() > 4096
                    || result.insert(key, value).is_some()
                {
                    return Err(serde::de::Error::custom(
                        "startup environment duplicate/unbounded name/value",
                    ));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Environment)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupDynamicChannel {
    env: String,
    target_fd: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupProducts {
    gnu_slot: String,
    driver_slot: String,
    nvml_member: String,
    provider_checks: String,
    gnu_manifest_sha256: String,
    driver_manifest_sha256: String,
    tinygrad_manifest_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupPolicy {
    schema: String,
    primary_device_role: String,
    #[serde(deserialize_with = "required_startup_nullable_policy")]
    l4_query_policy: Option<StartupL4Policy>,
}

#[derive(Debug, Deserialize)]
enum StartupL4Policy {
    #[serde(rename = "l4-is-mig-success-zero-mode-not-supported-v1")]
    SuccessZero,
    #[serde(rename = "l4-is-mig-not-supported-mode-not-supported-v1")]
    NotSupported,
}

fn required_startup_nullable_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<StartupL4Policy>, D::Error> {
    Option::<StartupL4Policy>::deserialize(deserializer)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartupWire {
    target_fd: u32,
    preflight_version: u32,
    result_version: u32,
    init_version: u32,
    maximum_frame_bytes: u32,
}

// Retained static product inputs only. No materialization, current assignment,
// startup UUID, driver-close or admission proof is constructed here. The SAME
// actual materializations stay rooted by PreparedTrustedSessionCustody.
struct OriginalProductInputs {
    gnu: OriginalSelectedProductInputs,
    driver: OriginalSelectedProductInputs,
    devices: std::sync::Arc<lillux::CharacterDeviceSet>,
}

struct OriginalSelectedProductInputs {
    selection:
        ryeos_state::external_content::products::composition::ResolvedExternalProductSelection,
    witness: ryeos_state::external_content::products::publication::VerifiedProductWitness,
    manifest: ryeos_state::objects::ExternalLargeContentManifestObject,
}

/// Selected original expectation retained inside the existing input owner.
/// Generic readonly input verification does not require or create this value.
struct OriginalIsolationExpectation {
    runtime: std::sync::Arc<ryeos_engine::isolation::IsolationRuntime>,
    capture: Option<std::sync::Arc<ryeos_engine::isolation::VerifiedIsolationBackendCapture>>,
    original_deadline: lillux::time::MonotonicDeadline,
    state: OriginalIsolationExpectationState,
}

enum OriginalIsolationExpectationState {
    Pending,
    Verified {
        registered_root: std::path::PathBuf,
        current_executor: ryeos_engine::binary_resolver::BundleExecutorManifestIdentity,
    },
    Refused(String),
}

impl VerifiedTrustedPooledSessionInputs {
    /// Verify current retained bytes and independent node/product agreement.
    /// Existing executable prerequisite refusals remain authoritative.
    pub fn verify(state: &crate::state::AppState, capsule_hash: &str) -> Result<Self> {
        let capsule = state
            .state_store
            .admitted_persistent_session_capsule(capsule_hash)?;
        validate_capsule_current_trust(&state.engine, &capsule)?;
        let session = retained_session_protocol(&state.engine, &capsule)?;
        use ryeos_engine::protocols::descriptor::{
            PersistentSessionCleanupAuthority, PersistentSessionProcessMode,
        };
        anyhow::ensure!(
            session.process_mode == PersistentSessionProcessMode::PooledRequests
                && session.cleanup_authority
                    == PersistentSessionCleanupAuthority::TrustedProcessGroup
                && capsule.external_candidate.is_none(),
            "verified trusted pooled inputs require the exact local pooled product"
        );
        ryeos_engine::protocols::descriptor::validate_session_resource_cleanup_contract(&session)
            .map_err(|error| anyhow!(error))?;
        let product = session
            .resource_cleanup_contract
            .as_ref()
            .context("verified trusted pooled inputs have no product cleanup contract")?;
        let node = state
            .node_policy
            .require::<crate::node_policy::sections::execution::NodeExecutionAdmissionPolicy>(
        )?;
        let resources = node
            .resource_authority
            .as_ref()
            .context("verified trusted pooled inputs have no node resource authority")?;
        resources.validate()?;
        anyhow::ensure!(
            resources.trusted_cleanup_contract_digest.as_ref() == Some(product),
            "verified trusted pooled node/product cleanup contracts differ"
        );
        let exact = retained_exact_program(&capsule)?;
        validate_exact_evidence_attachments(&exact)?;
        anyhow::ensure!(
            exact
                .resolution_output
                .effective_definition_digest()?
                .as_str()
                == exact.effective_definition_digest,
            "verified trusted pooled exact program digest does not reproduce"
        );
        let ryeos_state::objects::AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            protocol_ref,
            protocol_content_hash,
            ..
        } = &capsule.artifact_identity
        else {
            bail!("verified trusted pooled capsule has a non-direct artifact identity");
        };
        let selected = crate::execution_realization::verify_persistent_session_selected(
            state,
            &capsule,
            &exact.resolution_output.restore(),
            &exact.effective_definition_digest,
            protocol_ref,
            protocol_content_hash,
        )?;
        anyhow::ensure!(
            !selected.selections().is_empty(),
            "resource-free sessions do not acquire trusted resource input proof"
        );
        let state_authority = state.state_store.pinned_state_authority()?;
        state_authority.runtime_directory().ensure_path_binding()?;
        let inputs = Self {
            state_store: std::sync::Arc::clone(&state.state_store),
            state_authority,
            capsule,
            exact,
            selected,
            cleanup_contract_digest: product.clone(),
            original_isolation: None,
            original_product_inputs: None,
            original_startup_contract: None,
            startup_contract_selection: session.startup_contract.clone(),
        };
        inputs.require_same_state(state)?;
        Ok(inputs)
    }

    /// One-use binding for the original trusted funding path only. The pool
    /// roots these SAME inputs before calling; failed validation preserves the
    /// actual snapshot and sticky refusal before any financial write/contact.
    pub(crate) fn bind_original_isolation_expectation(
        &mut self,
        state: &crate::state::AppState,
        budget: &crate::handler_context::OriginalExecutionBudget,
    ) -> Result<()> {
        if let Some(original) = self.original_isolation.as_mut() {
            let failure = "original isolation expectation cannot be replaced or retried".to_owned();
            original.state = OriginalIsolationExpectationState::Refused(failure.clone());
            bail!(failure);
        }
        self.original_isolation = Some(OriginalIsolationExpectation {
            runtime: std::sync::Arc::clone(&state.isolation),
            capture: state.isolation.verified_backend_capture().cloned(),
            original_deadline: budget.contact_expiry(),
            state: OriginalIsolationExpectationState::Pending,
        });
        let result = (|| {
            self.require_same_state_incarnation(state)?;
            let original = self
                .original_isolation
                .as_ref()
                .expect("snapshot retained before checks");
            let verified = verify_original_isolation_expectation(original)?;
            let products = self.authenticate_original_product_inputs(state)?;
            // Root the authenticated static inputs before any late refusal.
            self.original_product_inputs = Some(products);
            self.prepare_original_startup_contract(state)?;
            self.require_same_state_incarnation(state)?;
            let original = self
                .original_isolation
                .as_ref()
                .expect("original snapshot retained");
            anyhow::ensure!(
                verify_original_isolation_expectation(original)? == verified,
                "original product preparation changed protected generation identity"
            );
            Ok(verified)
        })();
        let original = self
            .original_isolation
            .as_mut()
            .expect("original snapshot remains retained");
        match result {
            Ok((registered_root, current_executor)) => {
                original.state = OriginalIsolationExpectationState::Verified {
                    registered_root,
                    current_executor,
                };
                Ok(())
            }
            Err(error) => {
                original.state = OriginalIsolationExpectationState::Refused(error.to_string());
                Err(error)
            }
        }
    }

    pub(crate) fn require_original_isolation_expectation(
        &self,
        state: &crate::state::AppState,
    ) -> Result<()> {
        self.require_same_state_incarnation(state)?;
        let original = self
            .original_isolation
            .as_ref()
            .context("selected original funding lacks isolation expectation")?;
        require_current_original_isolation_expectation(original, state)?;
        self.require_retained_original_product_inputs()?;
        require_original_isolation_unexpired(original)
    }

    /// Borrow the independently selected original expectation after its current
    /// protected-state checks. This is metadata custody, not native evidence.
    pub(crate) fn original_isolation_capture(
        &self,
        state: &crate::state::AppState,
    ) -> Result<&std::sync::Arc<ryeos_engine::isolation::VerifiedIsolationBackendCapture>> {
        self.require_original_isolation_expectation(state)?;
        self.original_isolation
            .as_ref()
            .and_then(|original| original.capture.as_ref())
            .context("verified original isolation capture disappeared")
    }

    /// Read-only retained inputs; none of these borrows grants execution.
    pub fn capsule(&self) -> &AdmittedPersistentSessionCapsule {
        &self.capsule
    }
    pub fn exact(&self) -> &PersistentSessionExactProgram {
        &self.exact
    }
    pub fn selected(&self) -> &crate::execution_resources::SelectedExecutionResources {
        &self.selected
    }
    pub fn cleanup_contract_digest(&self) -> &str {
        &self.cleanup_contract_digest
    }

    pub fn require_same_state(&self, state: &crate::state::AppState) -> Result<()> {
        self.require_same_state_incarnation(state)?;
        if let Some(original) = self.original_isolation.as_ref() {
            require_current_original_isolation_expectation(original, state)?;
            self.require_retained_original_product_inputs()?;
            require_original_isolation_unexpired(original)?;
        }
        Ok(())
    }

    fn require_same_state_incarnation(&self, state: &crate::state::AppState) -> Result<()> {
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&self.state_store, &state.state_store),
            "verified trusted pooled inputs belong to another StateStore owner"
        );
        let current = state.state_store.pinned_state_authority()?;
        let original = self.state_authority.runtime_directory();
        original.ensure_path_binding()?;
        current.runtime_directory().ensure_path_binding()?;
        anyhow::ensure!(
            original.identity()? == current.runtime_directory().identity()?,
            "verified trusted pooled inputs belong to another runtime incarnation"
        );
        Ok(())
    }

    fn authenticate_original_product_inputs(
        &self,
        state: &crate::state::AppState,
    ) -> Result<OriginalProductInputs> {
        self.capsule.validate()?;
        let original = self
            .original_isolation
            .as_ref()
            .context("original product preparation lacks original expectation")?;
        require_original_isolation_unexpired(original)?;
        self.require_same_state_incarnation(state)?;
        let selections = self
            .capsule
            .retained_product_selections
            .as_ref()
            .context("original product preparation lacks sealed product selections")?;
        let devices = self
            .selected
            .devices()
            .context("original product preparation lacks actual selected character-device owner")?;
        let limits = state
            .node_policy
            .require::<crate::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
            .closure_limits()?;
        let guard = self
            .state_authority
            .acquire_shared_guard_until(original.original_deadline)?;
        require_original_isolation_unexpired(original)?;
        self.state_authority.ensure_guard(&guard)?;
        let gnu = authenticate_original_selected_product(
            state,
            &self.state_authority,
            &guard,
            limits,
            selections
                .get("python-gnu")
                .context("original GNU product selection missing")?,
        )?;
        require_original_isolation_unexpired(original)?;
        let driver = authenticate_original_selected_product(
            state,
            &self.state_authority,
            &guard,
            limits,
            selections
                .get("driver")
                .context("original driver product selection missing")?,
        )?;
        require_original_isolation_unexpired(original)?;
        self.require_same_state_incarnation(state)?;
        Ok(OriginalProductInputs {
            gnu,
            driver,
            devices: std::sync::Arc::clone(devices),
        })
    }

    fn require_retained_original_product_inputs(&self) -> Result<()> {
        let retained = self
            .original_product_inputs
            .as_ref()
            .context("original product inputs remain unprepared")?;
        let devices = self
            .selected
            .devices()
            .context("original product inputs lost selected device owner")?;
        anyhow::ensure!(
            std::sync::Arc::ptr_eq(&retained.devices, devices),
            "original product inputs differ from actual selected device owner"
        );
        let selections = self
            .capsule
            .retained_product_selections
            .as_ref()
            .context("original product inputs lost sealed selections")?;
        for (id, product) in [("python-gnu", &retained.gnu), ("driver", &retained.driver)] {
            anyhow::ensure!(
                selections.get(id) == Some(&product.selection),
                "original product inputs differ from sealed selection"
            );
            crate::operator_external_content::product_composition::verify_witness_projection(
                &product.selection,
                &product.witness,
            )?;
            // Immutable private factory output already passed the canonical
            // product bounds verifier under the original pinned guard.
            product.manifest.validate()?;
        }
        self.require_retained_original_startup_contract()?;
        Ok(())
    }

    fn prepare_original_startup_contract(&mut self, state: &crate::state::AppState) -> Result<()> {
        let session = retained_session_protocol(&state.engine, &self.capsule)?;
        anyhow::ensure!(
            session.startup_contract == self.startup_contract_selection,
            "startup selector changed from exact verified signed protocol"
        );
        let Some(selection) = session.startup_contract else {
            return Ok(());
        };
        let original = self
            .original_isolation
            .as_ref()
            .context("startup contract lacks original clock")?;
        require_original_isolation_unexpired(original)?;
        self.require_same_state_incarnation(state)?;
        let resolution = self.exact.resolution_output.restore();
        let source = crate::source_closure_admission::recover_source_closure(
            state,
            &state.engine,
            &resolution,
        )?
        .context("signed startup contract lacks admitted Worker source")?;
        self.original_startup_contract = Some(OriginalStartupContract {
            selection,
            source,
            member: None,
            realizations: None,
            contract: None,
            verified: false,
        });
        // Deposit actual recovered source first. Recovery's returned authority
        // is independently joined to SAME original guard inside the blob loader.
        let deadline = self
            .original_isolation
            .as_ref()
            .expect("same original budget")
            .original_deadline;
        let guard = self.state_authority.acquire_shared_guard_until(deadline)?;
        require_original_isolation_unexpired(
            self.original_isolation
                .as_ref()
                .expect("same original budget"),
        )?;
        self.state_authority.ensure_guard(&guard)?;
        let owner = self
            .original_startup_contract
            .as_mut()
            .expect("source retained before checks");
        owner.member = Some(
            crate::source_closure_admission::load_selected_startup_contract_member(
                &owner.source,
                &owner.selection,
                &guard,
            )?,
        );
        let member = owner
            .member
            .as_ref()
            .expect("bytes retained before hash/parse");
        anyhow::ensure!(
            member.bytes.len() as u64 == member.file.size
                && lillux::sha256_hex(&member.bytes) == owner.selection.sha256,
            "startup contract bytes differ from signed source member"
        );
        owner.contract =
            Some(serde_json::from_slice(&member.bytes).context("decode closed startup contract")?);
        let value = resolution
            .composed
            .derived
            .get(ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY)
            .context("startup contract lacks sealed external realization envelope")?;
        owner.realizations = Some(
            ryeos_engine::external_realization::RealizedExternalContentSet::from_value(value)?,
        );
        let limits = state
            .node_policy
            .require::<crate::node_policy::sections::object_closure::NodeObjectClosurePolicy>()?
            .closure_limits()?;
        let tinygrad = owner
            .realizations
            .as_ref()
            .expect("actual set retained before verification")
            .iter()
            .find(|entry| entry.id == "tinygrad")
            .context("startup contract lacks actual tinygrad realization")?;
        anyhow::ensure!(
            tinygrad.kind == ryeos_state::objects::ExternalContentKind::Tree
                && tinygrad.mount == "tinygrad"
                && tinygrad.mount_root == ryeos_state::objects::ExternalContentMountRoot::Project,
            "startup tinygrad realization has wrong shape/mount"
        );
        let cas = self.state_authority.cas_store()?;
        let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
            &cas,
            [tinygrad.manifest_hash.clone()],
            limits,
        )?;
        anyhow::ensure!(
            closure.is_complete(),
            "startup tinygrad actual CAS closure incomplete"
        );
        if let Some(manifest) =
            ryeos_state::objects::load_if_large_content_manifest(&cas, &tinygrad.manifest_hash)?
        {
            anyhow::ensure!(
                !manifest.is_file_shaped()
                    && manifest.entry_count == tinygrad.entry_count
                    && manifest.total_bytes == tinygrad.total_bytes,
                "startup tinygrad large manifest differs from sealed tree realization"
            );
            let store = self.state_authority.large_object_store()?;
            for entry in &manifest.entries {
                if entry.file_sha256.is_some() {
                    store.verify_manifest_commitment(entry)?;
                }
            }
        } else {
            let verified =
                ryeos_state::VerifiedExternalContentClosure::load(&cas, &tinygrad.manifest_hash)?;
            anyhow::ensure!(
                verified.manifest().entry_count == tinygrad.entry_count
                    && verified.manifest().total_bytes == tinygrad.total_bytes,
                "startup tinygrad manifest differs from sealed realization"
            );
        }
        self.state_authority.ensure_guard(&guard)?;
        self.validate_original_startup_contract()?;
        self.require_same_state_incarnation(state)?;
        require_original_isolation_unexpired(
            self.original_isolation
                .as_ref()
                .expect("same original clock"),
        )?;
        self.original_startup_contract
            .as_mut()
            .expect("actual owner retained")
            .verified = true;
        Ok(())
    }

    fn validate_original_startup_contract(&self) -> Result<()> {
        let owner = self
            .original_startup_contract
            .as_ref()
            .context("startup source owner absent")?;
        let contract = owner
            .contract
            .as_ref()
            .context("startup contract remains unparsed")?;
        anyhow::ensure!(
            contract.schema == "ryeos.local_tinygrad_current_assignment_startup.v1"
                && contract.role == "current_assignment_nvml_preflight_v1",
            "startup role/schema unsupported"
        );
        anyhow::ensure!(
            self.startup_contract_selection.as_ref() == Some(&owner.selection),
            "startup signed selector differs from verified capsule"
        );
        let ryeos_state::objects::SourceLogicalBinding::Worker { root, entry } =
            &owner.source.binding().logical_binding
        else {
            bail!("startup source is not an ordinary typed-source Worker");
        };
        anyhow::ensure!(
            &contract.entry.root == root && &contract.entry.path == entry,
            "startup entry differs from actual Worker binding"
        );
        require_startup_source_member(&owner.source, &contract.entry)?;
        anyhow::ensure!(
            (1..=64).contains(&contract.bootstrap_members.len()),
            "startup bootstrap member count invalid"
        );
        let mut previous: Option<(&str, &str)> = None;
        let mut has_entry = false;
        for member in &contract.bootstrap_members {
            let key = (member.root.as_str(), member.path.as_str());
            anyhow::ensure!(
                previous.is_none_or(|old| old < key),
                "startup bootstrap members duplicate/unsorted"
            );
            previous = Some(key);
            require_startup_source_member(&owner.source, member)?;
            has_entry |= member.root == contract.entry.root
                && member.path == contract.entry.path
                && member.sha256 == contract.entry.sha256;
        }
        anyhow::ensure!(has_entry, "startup bootstrap omits actual entry");
        let ryeos_state::objects::AdmittedExecutionClosure::DirectItemExecutor {
            execution_plan,
            ..
        } = &self.capsule.execution_closure
        else {
            bail!("startup contract lacks exact direct launch plan");
        };
        let plan: ryeos_engine::contracts::ExecutionPlan =
            serde_json::from_value(execution_plan.clone())?;
        anyhow::ensure!(
            matches!(
                plan.endpoint_requirement,
                ryeos_engine::contracts::ExecutionEndpointRequirement::Local {}
            ) && plan.external_endpoint_binding.is_none()
                && self.capsule.external_candidate.is_none(),
            "startup role requires actual local placement"
        );
        let mut subprocesses = plan.nodes.iter().filter_map(|node| match node {
            ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } => {
                Some(spec.as_ref())
            }
            _ => None,
        });
        let spec = subprocesses
            .next()
            .context("startup launch lacks subprocess")?;
        anyhow::ensure!(
            subprocesses.next().is_none(),
            "startup launch has alternate subprocess"
        );
        let code = spec
            .verified_command
            .as_ref()
            .context("startup command lacks actual admitted executable")?
            .code();
        anyhow::ensure!(
            contract.launch.command_sha256 == code.content_hash
                && contract.launch.args == spec.args
                && (1..=32).contains(&contract.launch.args.len()),
            "startup command/typed args differ from retained plan"
        );
        let mut entry_args = 0;
        for arg in &contract.launch.args {
            match arg {
                ryeos_engine::contracts::PlanArgument::AdmittedSourceEntry => entry_args += 1,
                ryeos_engine::contracts::PlanArgument::Literal { value }
                    if value.len() <= 1024 && !value.chars().any(char::is_control) => {}
                _ => bail!("startup argument authorizes alternate/unbounded source"),
            }
        }
        let retained_env: std::collections::BTreeMap<String, String> = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        anyhow::ensure!(
            entry_args == 1
                && contract.launch.env.len() <= 32
                && contract.launch.env == retained_env,
            "startup static environment differs from retained plan"
        );
        anyhow::ensure!(
            lillux::canonical_json(&serde_json::to_value(&contract.launch.env)?)
                .map_err(anyhow::Error::msg)?
                .len()
                <= 4096
                && contract.launch.env.iter().all(|(k, v)| !k.is_empty()
                    && k.len() <= 128
                    && v.len() <= 4096
                    && !k.chars().any(char::is_control)
                    && !v.chars().any(char::is_control)),
            "startup environment unbounded"
        );
        let dynamic = &contract.launch.dynamic_channels;
        anyhow::ensure!(
            dynamic.len() == 2
                && dynamic[0].env == self.capsule.wire.channel_env
                && dynamic[0].target_fd == 0
                && dynamic[1].env == cleanup_channel::CHANNEL_ENV
                && dynamic[1].target_fd == cleanup_channel::TARGET_FD
                && dynamic[0].env != dynamic[1].env
                && dynamic
                    .iter()
                    .all(|item| !contract.launch.env.contains_key(&item.env)),
            "startup dynamic FD/env declaration differs"
        );
        let environment =
            lillux::canonical_json(&serde_json::to_value(&self.capsule.process_environment)?)
                .map_err(anyhow::Error::msg)?;
        require_startup_dynamic_environment_keys(
            self.capsule.process_environment.keys().map(String::as_str),
            &retained_env,
            &dynamic[0].env,
            &dynamic[1].env,
        )?;
        anyhow::ensure!(
            contract.launch.process_environment_sha256
                == lillux::sha256_hex(environment.as_bytes()),
            "startup declared dynamic realization environment differs from capsule"
        );
        let products = self
            .original_product_inputs
            .as_ref()
            .context("startup role lacks actual retained products/device")?;
        let declared = &contract.products;
        anyhow::ensure!(
            declared.gnu_slot == "python-gnu"
                && declared.driver_slot == "driver"
                && declared.nvml_member == "lib/libnvidia-ml.so.1"
                && declared.provider_checks == "trusted-gnu-loader-provider-and-function-origin-v1"
                && declared.gnu_manifest_sha256 == products.gnu.selection.manifest_hash
                && declared.driver_manifest_sha256 == products.driver.selection.manifest_hash,
            "startup selected GNU/driver provider envelope differs"
        );
        let tinygrad = owner
            .realizations
            .as_ref()
            .context("startup external envelope missing")?
            .iter()
            .find(|entry| entry.id == "tinygrad")
            .context("startup tinygrad realization missing")?;
        anyhow::ensure!(
            declared.tinygrad_manifest_sha256 == tinygrad.manifest_hash,
            "startup tinygrad envelope differs"
        );
        let realizations = owner
            .realizations
            .as_ref()
            .expect("actual envelope retained");
        for (id, product) in [("python-gnu", &products.gnu), ("driver", &products.driver)] {
            let actual = realizations
                .iter()
                .find(|entry| entry.id == id)
                .context("startup product realization missing")?;
            require_startup_realization_coordinates(
                actual,
                &product.selection.declaration,
                &product.selection.manifest_hash,
                product.witness.evidence.entry_count,
                product.witness.evidence.total_bytes,
            )?;
        }
        require_startup_gnu_launch_coordinates(
            &spec.args,
            &retained_env,
            products.gnu.selection.declaration.mount_root,
            &products.gnu.selection.declaration.mount,
            products.driver.selection.declaration.mount_root,
            &products.driver.selection.declaration.mount,
            tinygrad.mount_root,
            &tinygrad.mount,
        )?;
        // These are actual authenticated selected manifests, not merely matching
        // declared hashes. Canonical internal symlinks remain qualified-product
        // semantics; the independently admitted bootstrap still checks origins.
        for path in [
            "python/bin/python3.14",
            "python/lib/ld-linux-x86-64.so.2",
            "python/lib/libc.so.6",
        ] {
            require_startup_executable_manifest_member(&products.gnu.manifest, path)?;
        }
        require_startup_executable_manifest_member(
            &products.driver.manifest,
            &declared.nvml_member,
        )?;
        anyhow::ensure!(
            products
                .gnu
                .manifest
                .entries
                .iter()
                .any(|entry| entry.path == "python/lib"
                    && entry.kind == ryeos_state::objects::ExternalContentManifestEntryKind::Dir),
            "startup GNU library namespace absent from actual manifest"
        );
        anyhow::ensure!(
            contract.policy.schema == "ryeos.local_tinygrad_nvml_assignment_policy.v1"
                && !contract.policy.primary_device_role.is_empty()
                && contract.policy.primary_device_role.len() <= 64
                && contract
                    .policy
                    .primary_device_role
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                && products
                    .devices
                    .identities()
                    .iter()
                    .any(|device| device.role == contract.policy.primary_device_role),
            "startup finite policy differs from actual selected device roles"
        );
        let wire = &contract.wire;
        anyhow::ensure!(
            wire.target_fd == 4
                && wire.preflight_version == 3
                && wire.result_version == 3
                && wire.init_version == 2
                && wire.maximum_frame_bytes == 4096,
            "startup wire contract differs"
        );
        Ok(())
    }

    fn require_retained_original_startup_contract(&self) -> Result<()> {
        if let Some(owner) = &self.original_startup_contract {
            anyhow::ensure!(
                owner.verified,
                "startup source/member validation remains refused"
            );
            self.validate_original_startup_contract()?;
        } else {
            anyhow::ensure!(
                self.startup_contract_selection.is_none(),
                "selected startup contract remains unprepared"
            );
        }
        Ok(())
    }
}

fn require_startup_source_member(
    source: &crate::source_closure_admission::AdmittedSourceClosure,
    member: &StartupSourceMember,
) -> Result<()> {
    let selection =
        ryeos_engine::protocols::descriptor::PersistentSessionStartupContractSelection {
            source_root: member.root.clone(),
            member: member.path.clone(),
            sha256: member.sha256.clone(),
        };
    selection.validate().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        source
            .manifest()
            .entries
            .iter()
            .any(|entry| entry.root == member.root
                && entry.path == member.path
                && entry.blob_hash == member.sha256),
        "startup bootstrap member differs from actual source manifest"
    );
    Ok(())
}

fn require_startup_realization_coordinates(
    actual: &ryeos_state::objects::ExternalContentRealization,
    declaration: &ryeos_state::external_content::products::composition::ExternalProductSlotDeclaration,
    manifest_hash: &str,
    entry_count: usize,
    total_bytes: u64,
) -> Result<()> {
    anyhow::ensure!(
        actual.id == declaration.id
            && actual.kind == declaration.kind
            && actual.mode == ryeos_state::objects::ExternalContentMode::Pinned
            && actual.manifest_hash == manifest_hash
            && actual.mount_root == declaration.mount_root
            && actual.mount == declaration.mount
            && actual.entry_count == entry_count
            && actual.total_bytes == total_bytes,
        "startup actual product realization contradicts selected coordinates"
    );
    Ok(())
}

// Finite grammar for this exact GNU Python compatibility product, not a second
// plan compiler or a constructor of executable/qualified owner authority.
fn require_startup_gnu_launch_coordinates(
    args: &[ryeos_engine::contracts::PlanArgument],
    env: &std::collections::BTreeMap<String, String>,
    gnu_root: ryeos_state::objects::ExternalContentMountRoot,
    gnu_mount: &str,
    driver_root: ryeos_state::objects::ExternalContentMountRoot,
    driver_mount: &str,
    tinygrad_root: ryeos_state::objects::ExternalContentMountRoot,
    tinygrad_mount: &str,
) -> Result<()> {
    use ryeos_state::objects::ExternalContentMountRoot::Project;
    anyhow::ensure!(
        gnu_root == Project
            && driver_root == Project
            && tinygrad_root == Project
            && gnu_mount == "python-gnu"
            && driver_mount == "driver"
            && tinygrad_mount == "tinygrad",
        "startup product roots/mounts differ from the finite GNU workspace layout"
    );
    let mut expected: Vec<_> = [
        format!("{gnu_mount}/python/lib/ld-linux-x86-64.so.2"),
        "--inhibit-cache".into(),
        "--library-path".into(),
        format!("{gnu_mount}/python/lib"),
        format!("{gnu_mount}/python/bin/python3.14"),
        "-P".into(),
        "-S".into(),
        "-B".into(),
        "-X".into(),
        "utf8".into(),
    ]
    .into_iter()
    .map(ryeos_engine::contracts::PlanArgument::literal)
    .collect();
    expected.push(ryeos_engine::contracts::PlanArgument::AdmittedSourceEntry);
    anyhow::ensure!(
        args == expected.as_slice(),
        "startup interpreter/loader argv is not the selected GNU recipe"
    );
    for (key, value) in [
        ("PYTHONHOME", format!("{gnu_mount}/python")),
        ("PYTHONHASHSEED", "0".into()),
        ("PYTHONDONTWRITEBYTECODE", "1".into()),
        ("PYTHONNOUSERSITE", "1".into()),
        ("PYTHONSAFEPATH", "1".into()),
        ("PYTHONUNBUFFERED", "1".into()),
        ("PATH", "".into()),
    ] {
        anyhow::ensure!(
            env.get(key) == Some(&value),
            "startup selected GNU environment coordinate differs: {key}"
        );
    }
    for key in env.keys() {
        let selected_python = [
            "PYTHONHOME",
            "PYTHONHASHSEED",
            "PYTHONDONTWRITEBYTECODE",
            "PYTHONNOUSERSITE",
            "PYTHONSAFEPATH",
            "PYTHONUNBUFFERED",
        ]
        .contains(&key.as_str());
        anyhow::ensure!(
            !startup_environment_override_name(key) || selected_python,
            "startup ambient loader/interpreter override refused: {key}"
        );
    }
    Ok(())
}

fn startup_environment_override_name(key: &str) -> bool {
    key.starts_with("LD_")
        || key.starts_with("PYTHON")
        || [
            "GLIBC_TUNABLES",
            "GCONV_PATH",
            "LOCPATH",
            "__PYVENV_LAUNCHER__",
        ]
        .contains(&key)
}

fn require_startup_dynamic_environment_keys<'a>(
    keys: impl IntoIterator<Item = &'a str>,
    retained_env: &std::collections::BTreeMap<String, String>,
    session_env: &str,
    cleanup_env: &str,
) -> Result<()> {
    for key in keys {
        anyhow::ensure!(
            !retained_env.contains_key(key)
                && key != session_env
                && key != cleanup_env
                && !startup_environment_override_name(key),
            "startup late environment overrides selected bootstrap/FD identity: {key}"
        );
    }
    Ok(())
}

fn require_startup_executable_manifest_member(
    manifest: &ryeos_state::objects::ExternalLargeContentManifestObject,
    path: &str,
) -> Result<()> {
    anyhow::ensure!(
        manifest.entries.iter().any(|entry| entry.path == path
            && matches!(
                entry.kind,
                ryeos_state::objects::ExternalContentManifestEntryKind::File
                    | ryeos_state::objects::ExternalContentManifestEntryKind::Symlink
            )),
        "startup selected provider executable/library member missing: {path}"
    );
    Ok(())
}

fn authenticate_original_selected_product(
    state: &crate::state::AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    limits: ryeos_state::object_closure::ObjectClosureLimits,
    selection: &ryeos_state::external_content::products::composition::ResolvedExternalProductSelection,
) -> Result<OriginalSelectedProductInputs> {
    use crate::operator_external_content::{
        product_composition, product_qualification, product_receipt,
    };
    selection.validate()?;
    // Principal and source are the already-admitted capsule's selection, not
    // caller-provided manifest metadata. Preserve recovery semantics; fresh
    // eligibility was checked under genuine ingress before capsule sealing.
    let witness = product_receipt::load_product_source(
        state,
        authority,
        guard,
        limits,
        &selection.owner_principal,
        &selection.witness_hash,
        &selection.witness_source,
        product_receipt::ProductSourceVerification::Retained,
    )?;
    product_composition::verify_witness_projection(selection, &witness)?;
    let qualification = selection
        .qualification
        .as_ref()
        .context("original selected product lacks admitted qualification")?;
    product_qualification::verify_retained_qualification_guarded(
        state,
        authority,
        guard,
        limits,
        qualification,
        &selection.owner_principal,
    )?;
    anyhow::ensure!(
        selection.manifest_kind == ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
            && selection.declaration.kind == ryeos_state::objects::ExternalContentKind::Tree,
        "original selected product requires retained large-content manifest"
    );
    let value = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        &authority.cas_store()?,
        &selection.manifest_hash,
        limits
            .max_object_bytes
            .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_BYTES as u64),
    )?;
    let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&value)?;
    anyhow::ensure!(
        !manifest.is_file_shaped()
            && manifest.entry_count == witness.evidence.entry_count
            && manifest.total_bytes == witness.evidence.total_bytes,
        "original product manifest contradicts retained tree witness"
    );
    ryeos_state::external_content::products::publication::verify_product_manifest_against_bounds(
        authority,
        &witness.evidence,
        &selection.relationship.required_product.bounds,
        limits,
        guard,
    )?;
    Ok(OriginalSelectedProductInputs {
        selection: selection.clone(),
        witness,
        manifest,
    })
}

fn require_original_isolation_unexpired(original: &OriginalIsolationExpectation) -> Result<()> {
    anyhow::ensure!(
        !original.original_deadline.has_elapsed(),
        "original isolation expectation deadline expired"
    );
    Ok(())
}

fn verify_original_isolation_expectation(
    original: &OriginalIsolationExpectation,
) -> Result<(
    std::path::PathBuf,
    ryeos_engine::binary_resolver::BundleExecutorManifestIdentity,
)> {
    require_original_isolation_unexpired(original)?;
    anyhow::ensure!(
        original.runtime.is_enforced(),
        "original isolation expectation requires enforced runtime"
    );
    let capture = original
        .capture
        .as_ref()
        .context("original isolation expectation lacks actual executable captures")?;
    let _operation = original
        .runtime
        .begin_registered_generation_operation()?
        .context(
            "original isolation expectation requires actual registered generation operation",
        )?;
    require_original_isolation_unexpired(original)?;
    original.runtime.ensure_registered_generation_current()?;
    let node_trust = original
        .runtime
        .registered_generation_node_trust()
        .context("original isolation expectation lacks independent retained node trust")?;
    let roots = original
        .runtime
        .registered_generation_roots()
        .context("original isolation expectation lacks registered bundle roots")?;
    let backend = capture.backend();
    capture.require_same_backend(backend)?;
    require_original_terminal_reporter_contract(
        backend.declaration.protocol,
        &backend.declaration.capabilities,
        &backend.effective_capabilities,
    )?;
    let mut selected_roots = roots
        .iter()
        .filter(|root| root.name == backend.selection.bundle);
    let root = selected_roots
        .next()
        .context("original isolation backend bundle is not registered")?;
    anyhow::ensure!(
        selected_roots.next().is_none(),
        "original isolation backend registration is ambiguous"
    );
    require_original_isolation_unexpired(original)?;
    let verified = ryeos_bundle::manifest::load_verified_manifest(
        &root.canonical_root.join(ryeos_engine::AI_DIR),
        &root.name,
        node_trust,
    )?;
    require_original_isolation_unexpired(original)?;
    anyhow::ensure!(
        verified.body_digest == backend.bundle_manifest_digest
            && verified.signer_fingerprint == backend.signer_fingerprint,
        "original isolation current Bundle identity differs from retained backend"
    );
    let mut declarations = verified
        .manifest
        .isolation_backends
        .iter()
        .filter(|declaration| declaration.id == backend.selection.implementation);
    let declaration = declarations
        .next()
        .context("original isolation selected declaration is missing")?;
    anyhow::ensure!(
        declarations.next().is_none() && declaration == &backend.declaration,
        "original isolation current signed declaration differs from retained backend"
    );
    // The current executor manifest reference is a DIFFERENT signed identity
    // from the Bundle YAML body. Old still-trusted executor proofs are not fresh.
    require_original_isolation_unexpired(original)?;
    let current_executor =
        ryeos_engine::binary_resolver::verify_bundle_executor_manifest_ref_identity(
            &root.canonical_root,
            node_trust,
        )?
        .context("original isolation current signed executor manifest reference is missing")?;
    require_original_isolation_unexpired(original)?;
    anyhow::ensure!(
        current_executor.signer_fingerprint == verified.signer_fingerprint,
        "original isolation current executor signer differs from Bundle signer"
    );
    for captured in std::iter::once(capture.adapter()).chain(capture.artifacts().values()) {
        require_original_isolation_unexpired(original)?;
        let executable = captured.captured();
        anyhow::ensure!(
            executable.identity.manifest_hash == current_executor.manifest_hash
                && executable.identity.signer_fingerprint == current_executor.signer_fingerprint,
            "original isolation retained executor differs from current signed executor reference"
        );
        let signer = node_trust
            .get(&executable.identity.signer_fingerprint)
            .context("original isolation executor signer is not independently node trusted")?;
        let identity = ryeos_engine::binary_resolver::BundlePayloadIdentity {
            content_hash: executable.identity.content_hash.clone(),
            manifest_hash: executable.identity.manifest_hash.clone(),
            item_source_hash: executable.identity.item_source_hash.clone(),
            signer_fingerprint: executable.identity.signer_fingerprint.clone(),
            target_triple: executable.identity.target_triple.clone(),
        };
        // Preserve the actual signed mode; it is source data, not a new grant.
        let (_, signed_mode) = ryeos_engine::executor_resolution::verify_executor_item_source(
            &executable.source_proof.item_source_object,
            &identity.item_source_hash,
            &executable.source_proof.selected_item_ref,
        )?;
        ryeos_engine::binary_resolver::verify_retained_bundle_payload_proof(
            &executable.source_proof,
            &identity,
            signed_mode,
            &signer.verifying_key,
        )?;
        require_original_isolation_unexpired(original)?;
    }
    capture.require_same_backend(backend)?;
    original.runtime.ensure_registered_generation_current()?;
    require_original_isolation_unexpired(original)?;
    Ok((root.canonical_root.clone(), current_executor))
}

fn require_current_original_isolation_expectation(
    original: &OriginalIsolationExpectation,
    state: &crate::state::AppState,
) -> Result<()> {
    require_original_isolation_unexpired(original)?;
    let (root, expected_executor) = match &original.state {
        OriginalIsolationExpectationState::Verified {
            registered_root,
            current_executor,
        } => (registered_root, current_executor),
        OriginalIsolationExpectationState::Pending => {
            bail!("original isolation expectation remains unverified")
        }
        OriginalIsolationExpectationState::Refused(failure) => {
            bail!("original isolation expectation was refused: {failure}")
        }
    };
    anyhow::ensure!(
        std::sync::Arc::ptr_eq(&original.runtime, &state.isolation),
        "original isolation expectation belongs to another protected runtime"
    );
    let expected = original
        .capture
        .as_ref()
        .context("original isolation retained capture disappeared")?;
    let actual = state
        .isolation
        .verified_backend_capture()
        .context("current original isolation runtime has no captures")?;
    anyhow::ensure!(
        std::sync::Arc::ptr_eq(expected, actual),
        "original isolation capture was replaced"
    );
    let _operation = original
        .runtime
        .begin_registered_generation_operation()?
        .context("original isolation contact requires actual generation operation")?;
    require_original_isolation_unexpired(original)?;
    original.runtime.ensure_registered_generation_current()?;
    require_original_isolation_unexpired(original)?;
    let node_trust = original
        .runtime
        .registered_generation_node_trust()
        .context("original isolation contact has no retained node trust")?;
    let actual_executor =
        ryeos_engine::binary_resolver::verify_bundle_executor_manifest_ref_identity(
            root, node_trust,
        )?
        .context("original isolation contact current executor reference disappeared")?;
    require_original_isolation_unexpired(original)?;
    anyhow::ensure!(
        &actual_executor == expected_executor,
        "original isolation current executor reference changed"
    );
    expected.require_same_backend(expected.backend())?;
    let backend = expected.backend();
    require_original_terminal_reporter_contract(
        backend.declaration.protocol,
        &backend.declaration.capabilities,
        &backend.effective_capabilities,
    )?;
    original.runtime.ensure_registered_generation_current()?;
    require_original_isolation_unexpired(original)?;
    Ok(())
}

// A metadata predicate inside the actual captured/current node-trust checks,
// never an issuer for arbitrary backend DTOs or native terminal evidence.
fn require_original_terminal_reporter_contract(
    protocol: ryeos_isolation_protocol::IsolationAdapterProtocolVersion,
    declared: &std::collections::BTreeSet<ryeos_isolation_protocol::IsolationCapability>,
    effective: &std::collections::BTreeSet<ryeos_isolation_protocol::IsolationCapability>,
) -> Result<()> {
    use ryeos_isolation_protocol::{IsolationAdapterProtocolVersion, IsolationCapability};
    anyhow::ensure!(
        protocol == IsolationAdapterProtocolVersion::Current,
        "original namespace reporter requires the current strict protocol"
    );
    let reporter = IsolationCapability::ProcessOriginalNamespaceTerminalReporting;
    anyhow::ensure!(
        declared.contains(&reporter) && effective.contains(&reporter),
        "original namespace reporter is not both node-declared and inspected"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::protocols::descriptor::PersistentSessionCleanupAuthority;

    #[test]
    fn startup_gnu_recipe_refuses_alternate_interpreter_loader_and_late_env_shadowing() {
        use ryeos_engine::contracts::PlanArgument;
        use ryeos_state::objects::ExternalContentMountRoot::{ExecutionRuntime, Project};
        // Raw comparison data only, not an admitted executable/source owner.
        let mut args: Vec<_> = [
            "python-gnu/python/lib/ld-linux-x86-64.so.2",
            "--inhibit-cache",
            "--library-path",
            "python-gnu/python/lib",
            "python-gnu/python/bin/python3.14",
            "-P",
            "-S",
            "-B",
            "-X",
            "utf8",
        ]
        .into_iter()
        .map(PlanArgument::literal)
        .collect();
        args.push(PlanArgument::AdmittedSourceEntry);
        let env: std::collections::BTreeMap<String, String> = [
            ("PYTHONHOME", "python-gnu/python"),
            ("PYTHONHASHSEED", "0"),
            ("PYTHONDONTWRITEBYTECODE", "1"),
            ("PYTHONNOUSERSITE", "1"),
            ("PYTHONSAFEPATH", "1"),
            ("PYTHONUNBUFFERED", "1"),
            ("PATH", ""),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
        require_startup_gnu_launch_coordinates(
            &args,
            &env,
            Project,
            "python-gnu",
            Project,
            "driver",
            Project,
            "tinygrad",
        )
        .unwrap();
        for (index, alternate) in [
            (0, "/lib64/ld-linux-x86-64.so.2"),
            (3, "/usr/lib"),
            (4, "/usr/bin/python3.14"),
        ] {
            let mut altered = args.clone();
            altered[index] = PlanArgument::literal(alternate);
            assert!(
                require_startup_gnu_launch_coordinates(
                    &altered,
                    &env,
                    Project,
                    "python-gnu",
                    Project,
                    "driver",
                    Project,
                    "tinygrad"
                )
                .is_err()
            );
        }
        for roots in [
            (ExecutionRuntime, Project, Project),
            (Project, ExecutionRuntime, Project),
            (Project, Project, ExecutionRuntime),
        ] {
            assert!(
                require_startup_gnu_launch_coordinates(
                    &args,
                    &env,
                    roots.0,
                    "python-gnu",
                    roots.1,
                    "driver",
                    roots.2,
                    "tinygrad"
                )
                .is_err()
            );
        }
        let mut audit = env.clone();
        audit.insert("LD_AUDIT".into(), "foreign.so".into());
        assert!(
            require_startup_gnu_launch_coordinates(
                &args,
                &audit,
                Project,
                "python-gnu",
                Project,
                "driver",
                Project,
                "tinygrad"
            )
            .is_err()
        );
        for key in [
            "PYTHONSAFEPATH",
            "PYTHONHASHSEED",
            "PYTHONNOUSERSITE",
            "PYTHONDONTWRITEBYTECODE",
            "LD_AUDIT",
            "RYEOS_SESSION_FD",
            "RYEOS_RESOURCE_CLEANUP_FD",
        ] {
            assert!(
                require_startup_dynamic_environment_keys(
                    [key],
                    &env,
                    "RYEOS_SESSION_FD",
                    "RYEOS_RESOURCE_CLEANUP_FD"
                )
                .is_err(),
                "{key}"
            );
        }
    }

    #[test]
    fn startup_actual_realization_comparison_refuses_wrong_mount_root_and_identity() {
        use ryeos_state::objects::{
            ExternalContentKind::Tree,
            ExternalContentMode::Pinned,
            ExternalContentMountRoot::{ExecutionRuntime, Project},
        };
        // Plain declared/realized DTO comparisons, no qualified witness fixture.
        let declaration =
            ryeos_state::external_content::products::composition::ExternalProductSlotDeclaration {
                id: "python-gnu".into(),
                relationship_ref: "config:fixture/relationship".into(),
                relationship: "gnu".into(),
                kind: Tree,
                mount_root: Project,
                mount: "python-gnu".into(),
            };
        let actual = ryeos_state::objects::ExternalContentRealization {
            id: "python-gnu".into(),
            kind: Tree,
            mode: Pinned,
            manifest_hash: "a".repeat(64),
            entry_count: 1,
            total_bytes: 1,
            mount_root: Project,
            mount: "python-gnu".into(),
        };
        require_startup_realization_coordinates(&actual, &declaration, &actual.manifest_hash, 1, 1)
            .unwrap();
        let mut wrong = actual.clone();
        wrong.mount_root = ExecutionRuntime;
        assert!(
            require_startup_realization_coordinates(
                &wrong,
                &declaration,
                &actual.manifest_hash,
                1,
                1
            )
            .is_err()
        );
        let mut wrong = actual.clone();
        wrong.mount = "alternate-gnu".into();
        assert!(
            require_startup_realization_coordinates(
                &wrong,
                &declaration,
                &actual.manifest_hash,
                1,
                1
            )
            .is_err()
        );
        assert!(
            require_startup_realization_coordinates(&actual, &declaration, &"b".repeat(64), 1, 1)
                .is_err()
        );
    }

    // Closed syntax fixtures only, never admitted source/device/provider owners.
    fn startup_contract_syntax() -> serde_json::Value {
        let member =
            serde_json::json!({"root":"source","path":"bootstrap.py","sha256":"a".repeat(64)});
        serde_json::json!({
            "schema":"ryeos.local_tinygrad_current_assignment_startup.v1", "role":"current_assignment_nvml_preflight_v1",
            "entry":member.clone(), "bootstrap_members":[member],
            "launch":{"command_sha256":"b".repeat(64),"args":[{"kind":"admitted_source_entry"}],"env":{},
                "process_environment_sha256":"c".repeat(64),"dynamic_channels":[{"env":"RYEOS_SESSION_FD","target_fd":0},{"env":"RYEOS_RESOURCE_CLEANUP_FD","target_fd":4}]},
            "products":{"gnu_slot":"python-gnu","driver_slot":"driver","nvml_member":"lib/libnvidia-ml.so.1",
                "provider_checks":"trusted-gnu-loader-provider-and-function-origin-v1","gnu_manifest_sha256":"d".repeat(64),
                "driver_manifest_sha256":"e".repeat(64),"tinygrad_manifest_sha256":"f".repeat(64)},
            "policy":{"schema":"ryeos.local_tinygrad_nvml_assignment_policy.v1","primary_device_role":"primary","l4_query_policy":null},
            "wire":{"target_fd":4,"preflight_version":3,"result_version":3,"init_version":2,"maximum_frame_bytes":4096}})
    }

    #[test]
    fn startup_contract_nested_objects_and_nullable_policy_are_closed() {
        let original = startup_contract_syntax();
        let parsed: StartupContract = serde_json::from_value(original.clone()).unwrap();
        assert!(parsed.policy.l4_query_policy.is_none());
        for path in [
            "",
            "/entry",
            "/bootstrap_members/0",
            "/launch",
            "/launch/dynamic_channels/0",
            "/products",
            "/policy",
            "/wire",
        ] {
            let mut value = original.clone();
            value
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), serde_json::json!(true));
            assert!(
                serde_json::from_value::<StartupContract>(value).is_err(),
                "{path}"
            );
        }
        let mut value = original.clone();
        value["policy"]
            .as_object_mut()
            .unwrap()
            .remove("l4_query_policy");
        assert!(serde_json::from_value::<StartupContract>(value).is_err());
        for policy in [
            serde_json::json!(false),
            serde_json::json!("automatic"),
            serde_json::json!([]),
        ] {
            let mut value = original.clone();
            value["policy"]["l4_query_policy"] = policy;
            assert!(serde_json::from_value::<StartupContract>(value).is_err());
        }
    }

    #[test]
    fn startup_contract_environment_refuses_duplicate_and_oversized_names() {
        let original = serde_json::to_string(&startup_contract_syntax()).unwrap();
        let duplicate = original.replace(
            "\"env\":{}",
            "\"env\":{\"DEV\":\"CUDA:PTX\",\"DEV\":\"CPU\"}",
        );
        assert!(serde_json::from_str::<StartupContract>(&duplicate).is_err());
        let mut value = startup_contract_syntax();
        value["launch"]["env"] = serde_json::json!({"DEV":true});
        assert!(serde_json::from_value::<StartupContract>(value).is_err());
        let mut value = startup_contract_syntax();
        value["launch"]["env"] = serde_json::Value::Object(
            (0..33)
                .map(|i| (format!("KEY{i}"), serde_json::json!("x")))
                .collect(),
        );
        assert!(serde_json::from_value::<StartupContract>(value).is_err());
    }

    #[test]
    fn original_reporter_contract_requires_declaration_and_inspection() {
        use ryeos_isolation_protocol::{IsolationAdapterProtocolVersion, IsolationCapability};
        let reporter = std::collections::BTreeSet::from([
            IsolationCapability::ProcessOriginalNamespaceTerminalReporting,
        ]);
        let absent = std::collections::BTreeSet::new();
        let current = IsolationAdapterProtocolVersion::Current;
        for (declared, effective) in [
            (&absent, &absent),
            (&reporter, &absent),
            (&absent, &reporter),
        ] {
            assert!(
                require_original_terminal_reporter_contract(current, declared, effective).is_err()
            );
        }
        require_original_terminal_reporter_contract(current, &reporter, &reporter).unwrap();
        // Metadata only: this does not manufacture captures, funding or native facts.
        assert!(
            serde_json::from_str::<IsolationAdapterProtocolVersion>(
                "\"ryeos.isolation-adapter/v12\""
            )
            .is_err()
        );
    }

    #[test]
    fn retained_predecessor_protocol_gets_only_its_historical_cleanup_meaning() {
        let exclusive = r#"
kind: protocol
name: structured_session
category: ryeos/core
abi_version: v1
stdin: { shape: opaque }
stdout: { shape: opaque_bytes, mode: terminal }
env_injections: []
capabilities:
  allows_pushed_head: false
  allows_target_site: false
  allows_detached: false
lifecycle: { mode: managed }
callback_channel: none
session:
  process_mode: exclusive_session
  workspace_authority: runtime_workspace
  network_authority: node_policy
  runtime_env_allowlist: []
  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY
  channel: inherited_unix_socket
  channel_env: RYEOS_SESSION_FD
  framing: u32_be_json
  wire_protocol: fixture.session
  wire_version: 1
  max_frame_bytes: 4096
"#;
        let descriptor = decode_retained_protocol_descriptor(exclusive).unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );

        let pooled = exclusive
            .replace("exclusive_session", "pooled_requests")
            .replace("runtime_workspace", "ephemeral_scratch")
            .replace("node_policy", "isolated")
            .replace(
                "  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY\n",
                "",
            );
        let descriptor = decode_retained_protocol_descriptor(&pooled).unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::NotRequired
        );

        // Current authoring never receives the retained-document bridge.
        assert!(
            serde_yaml::from_str::<ryeos_engine::protocols::ProtocolDescriptor>(exclusive).is_err()
        );
    }

    #[test]
    fn retained_predecessor_protocol_is_verified_before_historical_interpretation() {
        let key = lillux::crypto::SigningKey::from_bytes(&[91u8; 32]);
        let fingerprint = lillux::signature::compute_fingerprint(&key.verifying_key());
        let trust = ryeos_engine::trust::TrustStore::from_signers(vec![
            ryeos_engine::trust::TrustedSigner {
                fingerprint: fingerprint.clone(),
                verifying_key: key.verifying_key(),
                label: None,
            },
        ]);
        let body = r#"kind: protocol
name: structured_session
category: ryeos/core
abi_version: v1
stdin: { shape: opaque }
stdout: { shape: opaque_bytes, mode: terminal }
env_injections: []
capabilities:
  allows_pushed_head: false
  allows_target_site: false
  allows_detached: false
lifecycle: { mode: managed }
callback_channel: none
session:
  process_mode: exclusive_session
  workspace_authority: runtime_workspace
  network_authority: node_policy
  runtime_env_allowlist: []
  readiness_identity_env: RYEOS_SESSION_BOOT_IDENTITY
  channel: inherited_unix_socket
  channel_env: RYEOS_SESSION_FD
  framing: u32_be_json
  wire_protocol: fixture.session
  wire_version: 1
  max_frame_bytes: 4096
"#;
        let document = lillux::signature::sign_content(body, &key, "#", None);
        let content_hash = lillux::signature::content_hash(body);
        let descriptor = verify_and_decode_retained_protocol_document(
            &document,
            &content_hash,
            &fingerprint,
            &trust,
        )
        .unwrap();
        assert_eq!(
            descriptor.session.unwrap().cleanup_authority,
            PersistentSessionCleanupAuthority::LocalProcessScope
        );

        let tampered = document.replace("exclusive_session", "pooled_requests");
        assert!(
            verify_and_decode_retained_protocol_document(
                &tampered,
                &content_hash,
                &fingerprint,
                &trust,
            )
            .is_err()
        );
        assert!(
            verify_and_decode_retained_protocol_document(
                &document,
                &"0".repeat(64),
                &fingerprint,
                &trust,
            )
            .is_err()
        );
        let revoked = ryeos_engine::trust::TrustStore::from_signers(Vec::new());
        assert!(
            verify_and_decode_retained_protocol_document(
                &document,
                &content_hash,
                &fingerprint,
                &revoked,
            )
            .is_err()
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "persistent_session_admission/factory_controls.rs"]
mod factory_controls;

#[cfg(test)]
mod original_product_bounds_controls {
    use ryeos_state::external_content::products::*;

    // Raw product evidence and actual temporary CAS only. No signed witness,
    // qualification, original product input or startup proof is manufactured.
    #[test]
    fn canonical_original_product_directory_at_exact_depth_boundary_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let db = ryeos_state::StateDb::open(
            temp.path(),
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        let authority = db.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let cas = authority.cas_store().unwrap();
        let blob_hash = cas.store_blob(b"data").unwrap();
        let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(
            &serde_json::json!({
                "schema": ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
                "kind": ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
                "entry_count": 3, "total_bytes": 4,
                "entries": [
                    { "path": "lib", "kind": "dir" },
                    { "path": "lib/sub", "kind": "dir" },
                    { "path": "lib/sub/provider.so", "kind": "file", "mode": 420,
                      "blob_hash": blob_hash, "size": 4 }
                ]
            }),
        )
        .unwrap();
        let manifest_hash = cas.store_object(&manifest.to_value().unwrap()).unwrap();
        let declaration = ProductDeclaration {
            name: "runtime".into(),
            source: ProductSource::RetainedProject {},
            path: "products/runtime".into(),
            shape: ProductShape::Tree,
            storage: ProductStorage::LargeContent,
            required: true,
            bounds: ProductBounds {
                maximum_entries: 8,
                maximum_depth: 4,
                maximum_file_bytes: 1024,
                maximum_total_bytes: 4096,
            },
            expected_manifest_hash: None,
        };
        let declarations = ProductDeclarations {
            schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
            output_roots: Vec::new(),
            products: vec![declaration.clone()],
        };
        let producer = ProductProducerAdmission {
            canonical_ref: "tool:test/build".into(),
            effective_definition_digest: "1".repeat(64),
            exact_program_hash: "2".repeat(64),
            producer_project_snapshot_hash: "3".repeat(64),
            launch_authority_digest: "4".repeat(64),
            admitted_parameters_digest: "5".repeat(64),
        };
        let evidence = ProductCaptureEvidence {
            schema: PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
            owner_principal: format!("fp:{}", "a".repeat(64)),
            chain_root_id: "T-build-root".into(),
            thread_id: "T-depth-boundary".into(),
            admitted_launch_capsule_hash: "b".repeat(64),
            root_producer: producer.clone(),
            producer,
            result_project_snapshot_hash: "c".repeat(64),
            workspace_output_capture_hash: None,
            producer_partition_identity: None,
            recipe_binding: "build_recipe".into(),
            recipe_purpose: ProductRecipePurpose::GeneralProductV1,
            recipe_ref: "config:test/build".into(),
            recipe_raw_content_digest: "d".repeat(64),
            declarations_hash: declarations.content_hash().unwrap(),
            declarations,
            relationships: composition::ProductRelationships::empty(),
            declaration,
            capture_policy_digest: "e".repeat(64),
            manifest_hash,
            manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.into(),
            entry_count: 3,
            total_bytes: 4,
        };
        evidence.validate().unwrap();
        let mut bounds = evidence.declaration.bounds.clone();
        bounds.maximum_depth = 2;
        let error = publication::verify_product_manifest_against_bounds(
            &authority,
            &evidence,
            &bounds,
            ryeos_state::object_closure::ObjectClosureLimits::default(),
            &guard,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("relationship depth bound at lib/sub"));
        // Same actual CAS, evidence and guard: depth three permits the tree.
        bounds.maximum_depth = 3;
        publication::verify_product_manifest_against_bounds(
            &authority,
            &evidence,
            &bounds,
            ryeos_state::object_closure::ObjectClosureLimits::default(),
            &guard,
        )
        .unwrap();
    }
}
