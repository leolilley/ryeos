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
        let guard = self.state_authority.acquire_shared_guard()?;
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
        Ok(())
    }
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
