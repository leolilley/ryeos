//! Local admission fixtures with signed source and seeded historical node
//! testimony. These tests do not execute the checker or qualify a live runtime.

use super::*;
use anyhow::Result;
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_engine::trust::{TrustStore, TrustedSigner};
use ryeos_state::external_content::products::composition;
use ryeos_state::objects::*;
use serde_json::json;
use std::fs;

const AUX_VERIFIER: &str = "tool:qualification-fixture/auxiliary";
const AUX_POLICY: &str = "config:qualification-fixture/auxiliary";
const AUX_PINNED_VERIFIER: &str = "tool:qualification-fixture/auxiliary-pinned";
const AUX_PINNED_POLICY: &str = "config:qualification-fixture/auxiliary-pinned";
const TARGET_VERIFIER: &str = "tool:qualification-fixture/complete";
const TARGET_POLICY: &str = "config:qualification-fixture/complete";

fn relationship(
    name: &str,
    product: &str,
    slot: &str,
    policy: Option<&str>,
) -> composition::ProductRelationship {
    use ryeos_state::external_content::products::*;
    composition::ProductRelationship {
        name: name.into(),
        producer: composition::ProductRelationshipProducer {
            canonical_ref: "graph:qualification-fixture/build".into(),
            recipe_binding: "product_recipe".into(),
            product_name: product.into(),
            parameters: json!({}),
        },
        consumer: composition::ProductRelationshipConsumer {
            canonical_ref: TARGET_VERIFIER.into(),
            declaration_id: slot.into(),
        },
        required_product: composition::ProductRelationshipRequiredProduct {
            shape: ProductShape::Tree,
            storage: ProductStorage::Content,
            bounds: ProductBounds {
                maximum_entries: 4,
                maximum_depth: 2,
                maximum_file_bytes: 1024,
                maximum_total_bytes: 1024,
            },
        },
        qualification: composition::ProductRelationshipQualification {
            policy_ref: policy.map(str::to_owned),
            required_claims: policy
                .map(|p| {
                    vec![
                        if p == AUX_POLICY || p == AUX_PINNED_POLICY {
                            "fixture_auxiliary"
                        } else {
                            "fixture_complete"
                        }
                        .into(),
                    ]
                })
                .unwrap_or_default(),
        },
    }
}

fn fixture_product(
    fixture: &Fixture,
) -> Result<(
    String,
    ryeos_state::external_content::products::ProductCaptureEvidence,
)> {
    publish_fixture_product(
        fixture,
        "auxiliary",
        &fixture.manifest_hash,
        b"retained auxiliary input".len() as u64,
        composition::ProductRelationships::empty(),
    )
}

fn publish_fixture_product(
    fixture: &Fixture,
    name: &str,
    manifest_hash: &str,
    total_bytes: u64,
    relationships: composition::ProductRelationships,
) -> Result<(
    String,
    ryeos_state::external_content::products::ProductCaptureEvidence,
)> {
    use ryeos_state::external_content::products::*;
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let declaration = ProductDeclaration {
        name: name.into(),
        source: ProductSource::RetainedProject {},
        path: format!("products/{name}"),
        shape: ProductShape::Tree,
        storage: ProductStorage::Content,
        required: true,
        bounds: ProductBounds {
            maximum_entries: 4,
            maximum_depth: 2,
            maximum_file_bytes: 1024,
            maximum_total_bytes: 1024,
        },
        expected_manifest_hash: None,
    };
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: vec![],
        products: vec![declaration.clone()],
    };
    let producer = ProductProducerAdmission {
        canonical_ref: "graph:qualification-fixture/build".into(),
        effective_definition_digest: "1".repeat(64),
        exact_program_hash: "2".repeat(64),
        producer_project_snapshot_hash: "3".repeat(64),
        launch_authority_digest: "4".repeat(64),
        admitted_parameters_digest: canonical_value_digest(&json!({}))?,
    };
    let evidence = ProductCaptureEvidence {
        schema: PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: fixture.context.fingerprint.clone(),
        chain_root_id: "T-fixture-product-root".into(),
        thread_id: "T-fixture-product-terminal".into(),
        admitted_launch_capsule_hash: "5".repeat(64),
        root_producer: producer.clone(),
        producer,
        result_project_snapshot_hash: "6".repeat(64),
        workspace_output_capture_hash: None,
        producer_partition_identity: None,
        recipe_binding: "product_recipe".into(),
        recipe_purpose: ProductRecipePurpose::GeneralProductV1,
        recipe_ref: "config:qualification-fixture/products".into(),
        recipe_raw_content_digest: "7".repeat(64),
        declarations_hash: declarations.content_hash()?,
        declarations,
        relationships,
        declaration,
        capture_policy_digest: "8".repeat(64),
        manifest_hash: manifest_hash.into(),
        manifest_kind: EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        entry_count: 1,
        total_bytes,
    };
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&fixture.state.identity);
    let attestation = evidence.sign_attestation(&signer, "2026-09-29T00:00:00Z".into())?;
    let published = publication::publish_product_witness(
        &authority,
        &ProductCaptureCoordinate::from_evidence(&evidence)?,
        &attestation,
        fixture
            .state
            .node_policy
            .require::<NodeObjectClosurePolicy>()?
            .closure_limits()?,
        &signer,
        &guard,
    )?;
    Ok((published.witness.attestation_hash, evidence))
}

fn projectless_admission(
    fixture: &Fixture,
) -> Result<crate::thread_lifecycle::RootExecutionAdmission> {
    let plan_context = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: fixture.context.fingerprint.clone(),
            scopes: fixture.context.scopes.clone(),
        }),
        project_context: ProjectContext::None,
        subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        current_site_id: fixture.state.threads.site_id().into(),
        origin_site_id: fixture.state.threads.site_id().into(),
        execution_hints: ExecutionHints::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let project_binding = crate::thread_lifecycle::AdmittedProjectBinding::explicit_projectless(
        &fixture.state.engine,
        &plan_context,
    )?;
    Ok(crate::thread_lifecycle::preflight_root_execution(
        crate::thread_lifecycle::ResolveRootExecutionParams {
            engine: &fixture.state.engine,
            plan_context,
            project_binding,
            node_history_policy: fixture.state.node_history_policy()?,
            item_ref: AUX_VERIFIER,
            ref_bindings: BTreeMap::new(),
            product_selections: vec![],
            launch_mode: "wait",
            parameters: json!({}),
            usage_subject: None,
            usage_subject_asserted_by: None,
            creates_chain_root: true,
        },
    )?
    .root_admission)
}

fn finalized_current(
    fixture: &Fixture,
    current: &CurrentBundleVerifierIdentity,
) -> Result<ryeos_engine::effective_program::FinalizedEffectiveProgram> {
    let mut resolution = current.resolution.clone();
    let admitted = crate::external_content_admission::recover_external_realizations(
        &fixture.state,
        &resolution,
    )?
    .context("fixture current realization absent")?;
    let projection =
        ryeos_engine::effective_program::take_recovered_effective_program_derived(&mut resolution);
    let validation = fixture
        .state
        .engine
        .effective_validators
        .validate("tool", &resolution)?;
    let candidate = ryeos_engine::effective_program::relock_recovered_effective_program(
        resolution, validation, projection,
    )?;
    let roots = fixture.state.engine.resolution_roots(None);
    let contract = fixture
        .state
        .engine
        .kinds
        .get("tool")
        .unwrap()
        .external_content_contract();
    let finalization = ryeos_engine::effective_program::prove_finalization_authority(
        &candidate,
        contract,
        &[],
        &fixture.state.engine.launch_config_roots(&roots),
        None,
        Some(admitted.finalization_evidence()),
        None,
    )?;
    Ok(ryeos_engine::effective_program::finalize_effective_program(
        candidate,
        finalization,
    )?)
}

fn seed_auxiliary_qualification(
    fixture: &Fixture,
) -> Result<(String, ProductQualificationEvidence)> {
    seed_auxiliary_qualification_in_context(fixture, None)
}

fn seed_auxiliary_qualification_in_context(
    fixture: &Fixture,
    pinned: Option<&QualificationPinnedAdmissionContext<'_>>,
) -> Result<(String, ProductQualificationEvidence)> {
    use ryeos_state::external_content::products::qualification::*;
    use ryeos_state::external_content::qualification_purpose::{
        QUALIFICATION_LAUNCH_PURPOSE_SCHEMA, QualificationLaunchPurpose, QualificationSubject,
    };
    let logical_project_root = pinned.map(|_| Path::new(ADMITTED_DIRECT_PROJECT_ROOT));
    let verifier_ref = if pinned.is_some() {
        AUX_PINNED_VERIFIER
    } else {
        AUX_VERIFIER
    };
    let policy_ref = if pinned.is_some() {
        AUX_PINNED_POLICY
    } else {
        AUX_POLICY
    };
    let (witness_hash, product) =
        fixture_product(fixture).context("publish auxiliary product witness")?;
    let policy_source = resolve_current_bundle_qualification_policy(&fixture.state, policy_ref)
        .context("resolve auxiliary policy")?;
    let (admission, root_selections) = if let Some(pinned) = pinned {
        let selection = ProductSelection {
            declaration_id: "subject".into(),
            witness_hash: witness_hash.clone(),
            witness_source: ProductWitnessSource::LocalCapture {},
            qualification_hash: None,
        };
        let admission = crate::thread_lifecycle::preflight_root_execution(
            crate::thread_lifecycle::ResolveRootExecutionParams {
                engine: pinned.request_engine,
                plan_context: pinned.plan_context.clone(),
                project_binding: pinned.project_binding.clone(),
                node_history_policy: fixture.state.node_history_policy()?,
                item_ref: verifier_ref,
                ref_bindings: BTreeMap::new(),
                product_selections: vec![ProductSelectionInput {
                    target: ProductSelectionTarget::Root {},
                    selection: selection.clone(),
                }],
                launch_mode: "wait",
                parameters: json!({}),
                usage_subject: None,
                usage_subject_asserted_by: None,
                creates_chain_root: true,
            },
        )
        .context("preflight pinned auxiliary checker")?
        .root_admission;
        let mut resolution = admission.resolution_output().clone();
        let retained = crate::operator_external_content::product_composition::select_products_with_project_context_resolver(
            &fixture.state, &fixture.context, pinned.request_engine,
            &pinned.request_engine.resolution_roots(Some(pinned.project_root.to_path_buf())),
            &pinned.plan_context.subject_resolution_authority, &mut resolution, &[selection], None,
        ).context("admit pinned auxiliary unqualified subject")?;
        bind_selected_inputs(
            fixture,
            &resolution,
            &pinned.plan_context.subject_resolution_authority,
            &retained,
        )
        .context("bind pinned auxiliary subject")?;
        (admission, Some(retained))
    } else {
        (projectless_admission(fixture)?, None)
    };
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let current = resolve_current_bundle_verifier_identity_against_admitted(
        &fixture.state,
        &authority,
        &guard,
        &fixture.context,
        verifier_ref,
        &json!({}),
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(root_selections.as_ref()),
            logical_project_root,
            binding_subject_authority: pinned
                .map(|context| &context.plan_context.subject_resolution_authority),
            sealed_request: None,
            project_context_resolver: None,
            pinned_admission: pinned,
        },
        Some(admission.resolution_output()),
    )
    .context("reconstruct current auxiliary checker")?;
    let finalized =
        finalized_current(fixture, &current).context("relock finalized auxiliary checker")?;
    let purpose = QualificationLaunchPurpose {
        consumer_invocation: None,
        schema: QUALIFICATION_LAUNCH_PURPOSE_SCHEMA.into(),
        launch_id: format!("L-{}", "1".repeat(32)),
        owner_fingerprint: fixture.context.fingerprint.clone(),
        subject: QualificationSubject::CapturedProduct {
            product_witness_hash: witness_hash.clone(),
            witness_source: ProductWitnessSource::LocalCapture {},
            relationship_name: "auxiliary_to_consumer".into(),
        },
        policy_source: policy_source.clone(),
        consumer_definitions: None,
        consumer_content: None,
        producer_recipe_sources: BTreeMap::new(),
        remote_verifier_sources: BTreeMap::new(),
        subject_declaration_id: "subject".into(),
        subject_manifest_hash: fixture.manifest_hash.clone(),
        required_claims: vec!["fixture_auxiliary".into()],
        admitted_parameters_digest: policy_source.policy.admitted_parameters_digest()?,
        verifier_ref: verifier_ref.into(),
        verifier_effective_definition_digest: current.admitted_definition_digest.clone(),
        verifier_realized_definition_digest: current.effective_definition_digest.clone(),
    };
    let admission = admission
        .for_qualification(purpose)
        .context("attach exact auxiliary qualification purpose")?;
    let request = admission
        .execution_request(
            crate::thread_lifecycle::RootExecutionRoute::RootExecutorChain,
            "wait".into(),
            json!({}),
        )
        .context("resolve auxiliary execution request")?;
    let mut prepared = crate::thread_lifecycle::prepare_bundle_item_plan_for_qualification(
        &fixture.state.engine,
        &request,
        &fixture.state.isolation,
        &finalized,
        logical_project_root,
    )
    .context("prepare auxiliary direct plan")?;
    prepared.bind_logical_project_root(logical_project_root)?;
    let runtime_ref = prepared.runtime_ref()?.to_owned();
    let protocol = crate::thread_lifecycle::resolve_direct_terminator_protocol(
        &fixture.state.engine,
        &request,
    )
    .context("resolve auxiliary direct protocol")?;
    let artifact = prepared.admitted_artifact_identity(&request, protocol)?;
    anyhow::ensure!(
        artifact == current.artifact_identity,
        "fixture capsule artifact differs from current verifier"
    );
    let cas = authority.cas_store()?;
    let closure = prepared
        .admit_execution_closure(
            &cas,
            &fixture.state.isolation,
            protocol,
            &fixture.state.engine.node_trust_store,
            logical_project_root,
            Some(admission.project_authority()),
        )
        .context("admit auxiliary execution closure")?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::capture_finalized(
        &request,
        runtime_ref.clone(),
        &finalized,
        Some(&fixture.context),
    )
    .context("seal auxiliary finalized request")?;
    let mut capsule = AdmittedLaunchCapsule {
        schema: ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
        kind: "admitted_launch_capsule".into(),
        exact_program: sealed.admitted_program_value()?,
        exact_program_hash: sealed.admitted_program_hash()?,
        sealed_invocation: serde_json::to_value(&sealed)?,
        project_authority: admission.project_authority().clone(),
        lifecycle_authority: ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
        launch_driver: ExecutionLaunchDriver::DirectItemExecutor,
        artifact_identity: artifact.clone(),
        execution_closure: closure,
        execution_realization_hash: "0".repeat(64),
        source_binding_hash: None,
        accounting_scope: None,
        effective_caps: vec![],
        parent_delegation_caps: None,
        runtime_ref,
        executor_ref: request.executor_ref.clone(),
    };
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&fixture.state.identity);
    let identity = ExecutionIdentity {
        schema: EXECUTION_IDENTITY_SCHEMA_VERSION,
        kind: EXECUTION_IDENTITY_KIND.into(),
        daemon: ExecutionSubstrateBuild {
            version: "fixture".into(),
            revision: "fixture".into(),
            build_date: "2026-09-29".into(),
            profile: "test".into(),
        },
        operating_system: ExecutionOperatingSystemIdentity {
            family: "unix".into(),
            architecture: std::env::consts::ARCH.into(),
        },
        cpu: ExecutionCpuIdentity {
            model: None,
            features: vec![],
        },
        node_signer_fingerprint: fixture.state.identity.fingerprint().into(),
    };
    let identity_hash = cas.store_object(&identity.to_value()?)?;
    let substrate = Attestation::unsigned(
        identity_hash.clone(),
        EXECUTION_IDENTITY_ATTESTATION_CLAIM.into(),
        EXECUTION_IDENTITY_ATTESTATION_POLICY.into(),
        "2026-09-29T00:00:00Z".into(),
        None,
        json!({}),
    )
    .sign(&signer)?;
    let substrate_hash = cas.store_object(&substrate.to_value())?;
    let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        runtime_identity,
        executable_identity,
        protocol_ref,
        protocol_content_hash,
        ..
    } = &artifact
    else {
        unreachable!()
    };
    let executable_hash = match executable_identity {
        DirectExecutableIdentity::BundleExecutor { content_hash, .. }
        | DirectExecutableIdentity::CapturedContent { content_hash } => content_hash.clone(),
        DirectExecutableIdentity::NodePolicy => {
            anyhow::bail!("fixture checker lacks exact executable identity")
        }
    };
    let realization = AdmittedExecutionRealization {
        schema: EXECUTION_REALIZATION_SCHEMA_VERSION,
        kind: ADMITTED_EXECUTION_REALIZATION_KIND.into(),
        substrate_identity_hash: identity_hash.clone(),
        substrate_attestation_hash: substrate_hash,
        launch_authority_digest: capsule.launch_authority_digest()?,
        effective_definition_digest: current.effective_definition_digest.clone(),
        artifact_identity_digest: capsule.launch_authority().artifact_identity_digest()?,
        execution_closure_digest: capsule.launch_authority().execution_closure_digest()?,
        contract_ref: runtime_identity.runtime_ref.clone(),
        contract_digest: runtime_identity.runtime_content_hash.clone(),
        components: vec![ExecutionComponentReference {
            role: "executable".into(),
            content_digest: executable_hash.clone(),
            material: ExecutionComponentStorage::CasBlob {
                hash: executable_hash,
            },
        }],
        properties: BTreeMap::new(),
    };
    capsule.execution_realization_hash = cas.store_object(&realization.to_value()?)?;
    capsule.validate()?;
    let capsule_hash = cas.store_object(&capsule.to_value())?;
    let projector = fixture
        .state
        .engine
        .resolve_execution_evidence_projector(protocol_ref, protocol_content_hash)?
        .context("fixture protocol has no projector")?;
    let projector = &projector.projector;
    let result = ProductQualificationResult {
        schema: PRODUCT_QUALIFICATION_RESULT_SCHEMA.into(),
        subject_manifest_hash: fixture.manifest_hash.clone(),
        claims: vec!["fixture_auxiliary".into()],
        probe_evidence: json!({"fixture":"seeded_prior_node_testimony"}),
    };
    let evidence = ProductQualificationEvidence {
        schema: PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA.into(),
        product_witness_hash: witness_hash,
        witness_source: ProductWitnessSource::LocalCapture {},
        product_coordinate: ProductCaptureCoordinate::from_evidence(&product)?,
        verifier: ProductQualificationVerifier {
            chain_root_id: "T-fixture-verifier".into(),
            thread_id: "T-fixture-verifier".into(),
            admitted_launch_capsule_hash: capsule_hash,
            canonical_ref: verifier_ref.into(),
            effective_definition_digest: current.effective_definition_digest.clone(),
            exact_program_hash: capsule.exact_program_hash.clone(),
            admitted_parameters_digest: policy_source.policy.admitted_parameters_digest()?,
            launch_authority_digest: capsule.launch_authority_digest()?,
            execution_realization_hash: capsule.execution_realization_hash.clone(),
            artifact_identity: artifact.clone(),
            admitted_project_root: ProductQualificationVerifier::project_root_from_closure(
                &capsule.execution_closure,
            )?,
            substrate_identity_hash: identity_hash,
            subject_declaration_id: "subject".into(),
            subject_manifest_hash: fixture.manifest_hash.clone(),
            // Deliberately seeded historical node testimony. Fresh production
            // qualification must derive these from actual terminal/settlement
            // owners; current consumption authenticates the signed witness
            // and rechecks its retained capsule/source/artifact instead.
            terminal_snapshot_hash: "9".repeat(64),
            process_settlement_witness_digest: Some("8".repeat(64)),
            process_settlement_authority: Some(
                VerifierProcessSettlementAuthority::TrustedProcessGroupAbsent,
            ),
            result_digest: result.digest()?,
        },
        policy_source,
        consumer_content: None,
        qualification_worker_transcript_manifest_hash: None,
        verifier_root_selections: root_selections,
        execution_proof: ProductQualificationExecutionProof {
            projection_contract_ref: protocol_ref.clone(),
            projection_contract_digest: protocol_content_hash.clone(),
            projector: ProductQualificationProjectorIdentity {
                canonical_ref: projector.canonical_ref.clone(),
                descriptor_content_digest: projector.descriptor_content_digest.clone(),
                descriptor_signer_fingerprint: projector.descriptor_signer_fingerprint.clone(),
                binary_content_digest: projector.binary_content_digest.clone(),
                binary_manifest_digest: projector.binary_manifest_digest.clone(),
                binary_signer_fingerprint: projector.binary_signer_fingerprint.clone(),
            },
            participants: vec![],
            subordinate_attempt: None,
        },
        result,
    };
    evidence
        .validate()
        .context("validate auxiliary qualification evidence")?;
    let attestation = evidence.sign_attestation(&signer, "2026-09-29T00:00:01Z".into(), None)?;
    let published = publish_qualification_witness(
        &authority,
        &QualificationCoordinate::from_evidence(&evidence)?,
        &attestation,
        fixture
            .state
            .node_policy
            .require::<NodeObjectClosurePolicy>()?
            .closure_limits()?,
        &signer,
        &guard,
    )?;
    Ok((published.witness.attestation_hash, evidence))
}

struct Fixture {
    directory: tempfile::TempDir,
    state: AppState,
    context: HandlerContext,
    manifest_hash: String,
}

fn write_signed(path: &Path, source: &str, key: &SigningKey) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(
        path,
        lillux::signature::sign_content(source, key, "#", None),
    )?;
    Ok(())
}

fn fixture() -> Result<Fixture> {
    use std::os::unix::fs::PermissionsExt;
    let policy_schema =
        ryeos_state::external_content::products::qualification::PRODUCT_QUALIFICATION_POLICY_SCHEMA;
    let directory = tempfile::tempdir()?;
    let mut state = crate::state::test_support::build(directory.path())?;
    state.node_policy = Arc::new(crate::node_policy::NodePolicySnapshot::from_test_records(
        vec![
            Arc::new(NodeObjectClosurePolicy {
                schema: 1,
                max_roots: 256,
                max_objects: 32_768,
                max_blobs: 32_768,
                max_object_bytes: 32 * 1024 * 1024,
                max_total_object_bytes: 64 * 1024 * 1024,
                max_blob_bytes: 128 * 1024 * 1024,
                max_total_blob_bytes: 128 * 1024 * 1024,
                max_response_bytes: 256 * 1024 * 1024,
                max_links_per_object: 100_000,
                local_verification: None,
            }),
            Arc::new(ryeos_engine::history_policy::ResolvedNodeThreadHistoryPolicy::test_policy()),
        ],
    ));
    let operator = crate::identity::NodeIdentity::load(&state.config.operator_signing_key_path)?;
    let scopes = vec!["ryeos.execute.service.external-content/launch-product-qualification".into()];
    crate::identity::write_authorized_key_toml(
        &state.config.authorized_keys_dir,
        operator.fingerprint(),
        &base64::engine::general_purpose::STANDARD.encode(operator.verifying_key().as_bytes()),
        &scopes,
        "qualification fixture",
        "fixture",
        "2026-09-29T00:00:00Z",
        state.identity.signing_key(),
        crate::identity::WildcardPolicy::Reject,
    )?;
    let context = HandlerContext::new_with_authority(
        operator.principal_id(),
        scopes,
        true,
        Some(crate::identity::AuthorizedKeyPrincipalClass::LocalClient),
        None,
    );
    let authority = state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let bytes = b"retained auxiliary input";
    let blob = cas.store_blob(bytes)?;
    let manifest_hash = cas.store_object(&json!({
        "kind":EXTERNAL_CONTENT_MANIFEST_KIND,"schema":EXTERNAL_CONTENT_TREE_SCHEMA,
        "entries":[{"path":"input","kind":"file","mode":420,"blob_hash":blob,"size":bytes.len()}],
        "entry_count":1,"total_bytes":bytes.len(),
    }))?;
    let bundle = directory.path().join("fixture-bundle");
    let signer = SigningKey::from_bytes(&[53; 32]);
    write_signed(
        &bundle.join(".ai/manifest.yaml"),
        "name: qualification-fixture\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
        &signer,
    )?;
    let target = lillux::platform::current_binary_target()?;
    let executable = bundle.join(".ai/bin").join(&target).join("fixture-checker");
    fs::create_dir_all(executable.parent().unwrap())?;
    // The executable is never run. Its real bytes are captured through the
    // Bundle executor owner so current identity checks use an exact artifact.
    let executable_bytes = fs::read("/bin/true")?;
    fs::write(&executable, &executable_bytes)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    let bundle_cas = lillux::cas::CasStore::new(bundle.join(".ai/objects"));
    let executable_hash = bundle_cas.store_blob(&executable_bytes)?;
    let item_ref = format!("bin/{target}/fixture-checker");
    let item_source = json!({"kind":"item_source","item_ref":item_ref,"mode":493,
        "content_blob_hash":executable_hash,"integrity":format!("sha256:{executable_hash}"),"signature_info":null});
    let item_source_hash = bundle_cas.store_object(&item_source)?;
    write_signed(
        &executable.with_file_name("fixture-checker.item_source.json"),
        &lillux::canonical_json(&item_source)?,
        &signer,
    )?;
    let executor_manifest = bundle_cas.store_object(&json!({"kind":"source_manifest",
        "item_source_hashes":{(item_ref):item_source_hash}}))?;
    write_signed(
        &bundle.join(".ai/refs/bundles/manifest"),
        &format!(
            "{}\n{executor_manifest}\n",
            ryeos_engine::executor_resolution::EXECUTOR_MANIFEST_REF_DOMAIN
        ),
        &signer,
    )?;
    write_signed(
        &bundle.join(".ai/tools/qualification-fixture/auxiliary.yaml"),
        &format!(
            r#"category: qualification-fixture
name: auxiliary
version: '1.0.0'
description: Local source fixture checker
executor_id: '@subprocess'
execution_protocol: protocol:ryeos/core/opaque
effects: live
filesystem_authority: node_policy
network_authority: node_policy
external_content:
  - {{id: subject, kind: tree, mode: pinned, digest: {manifest_hash}, mount_root: execution_runtime, mount: auxiliary}}
config: {{command: 'bin:fixture-checker', args: [], input_data: '${{params_json}}', timeout_secs: 30}}
config_schema: {{type: object, properties: {{}}, additionalProperties: false}}
"#
        ),
        &signer,
    )?;
    write_signed(
        &bundle.join(".ai/config/qualification-fixture/auxiliary.yaml"),
        &format!(
            r#"category: qualification-fixture
name: auxiliary
version: '1.0.0'
product_qualification_policy:
  schema: {policy_schema}
  verifier_ref: {AUX_VERIFIER}
  subject_declaration_id: subject
  allowed_claims: [fixture_auxiliary]
  minimum_verifier_process_settlement: trusted_process_group_absent
  verifier_parameters: {{}}
"#
        ),
        &signer,
    )?;
    write_signed(
        &bundle.join(".ai/tools/qualification-fixture/complete.yaml"),
        r#"category: qualification-fixture
name: complete
version: '1.0.0'
description: Local complete-input source fixture checker
executor_id: '@subprocess'
execution_protocol: protocol:ryeos/core/opaque
effects: live
filesystem_authority: node_policy
network_authority: node_policy
external_content: []
external_product_slots:
  - {id: auxiliary, relationship_ref: 'config:qualification-fixture/relationships', relationship: auxiliary_to_consumer, kind: tree, mount_root: execution_runtime, mount: auxiliary}
  - {id: subject, relationship_ref: 'config:qualification-fixture/relationships', relationship: subject_to_checker, kind: tree, mount_root: execution_runtime, mount: subject}
config: {command: 'bin:fixture-checker', args: [], input_data: '${params_json}', timeout_secs: 30}
config_schema: {type: object, properties: {}, additionalProperties: false}
"#,
        &signer,
    )?;
    write_signed(
        &bundle.join(".ai/config/qualification-fixture/complete.yaml"),
        &format!(
            r#"category: qualification-fixture
name: complete
version: '1.0.0'
product_qualification_policy:
  schema: {policy_schema}
  verifier_ref: {TARGET_VERIFIER}
  subject_declaration_id: subject
  allowed_claims: [fixture_complete]
  minimum_verifier_process_settlement: trusted_process_group_absent
  verifier_parameters: {{}}
"#
        ),
        &signer,
    )?;
    write_signed(
        &bundle.join(".ai/tools/qualification-fixture/auxiliary-pinned.yaml"),
        r#"category: qualification-fixture
name: auxiliary-pinned
version: '1.0.0'
description: Local pinned auxiliary source fixture checker
executor_id: '@subprocess'
execution_protocol: protocol:ryeos/core/opaque
effects: live
filesystem_authority: node_policy
network_authority: node_policy
external_content: []
external_product_slots:
  - {id: subject, relationship_ref: 'config:qualification-fixture/relationships', relationship: auxiliary_to_checker, kind: tree, mount_root: execution_runtime, mount: auxiliary}
config: {command: 'bin:fixture-checker', args: [], input_data: '${params_json}', timeout_secs: 30}
config_schema: {type: object, properties: {}, additionalProperties: false}
"#,
        &signer,
    )?;
    write_signed(
        &bundle.join(".ai/config/qualification-fixture/auxiliary-pinned.yaml"),
        &format!(
            r#"category: qualification-fixture
name: auxiliary-pinned
version: '1.0.0'
product_qualification_policy:
  schema: {policy_schema}
  verifier_ref: {AUX_PINNED_VERIFIER}
  subject_declaration_id: subject
  allowed_claims: [fixture_auxiliary]
  minimum_verifier_process_settlement: trusted_process_group_absent
  verifier_parameters: {{}}
"#
        ),
        &signer,
    )?;
    let mut auxiliary_inner = relationship("auxiliary_to_checker", "auxiliary", "subject", None);
    auxiliary_inner.consumer.canonical_ref = AUX_PINNED_VERIFIER.into();
    let relationships = composition::ProductRelationships {
        schema: composition::PRODUCT_RELATIONSHIPS_SCHEMA.into(),
        relationships: vec![
            relationship(
                "auxiliary_to_consumer",
                "auxiliary",
                "auxiliary",
                Some(AUX_PINNED_POLICY),
            ),
            relationship("subject_to_checker", "subject", "subject", None),
            auxiliary_inner,
        ],
    };
    write_signed(
        &bundle.join(".ai/config/qualification-fixture/relationships.yaml"),
        &format!(
            "category: qualification-fixture\nname: relationships\nversion: '1.0.0'\nproduct_relationships: {}\n",
            serde_json::to_string(&relationships)?
        ),
        &signer,
    )?;
    let core = ryeos_engine::test_support::core_bundle_root();
    let standard = ryeos_engine::test_support::standard_bundle_root();
    let roots = vec![core.clone(), standard.clone(), bundle];
    let mut trust = ryeos_engine::test_support::live_trust_store();
    trust.extend_from(&TrustStore::from_signers(vec![TrustedSigner {
        fingerprint: lillux::signature::compute_fingerprint(&signer.verifying_key()),
        verifying_key: signer.verifying_key(),
        label: Some("qualification fixture".into()),
    }]));
    let kinds = ryeos_engine::kind_registry::KindRegistry::load_base(
        &[
            core.join(".ai/node/engine/kinds"),
            standard.join(".ai/node/engine/kinds"),
        ],
        &trust,
    )?;
    let (parsers, _) = ryeos_engine::parsers::ParserRegistry::load_base(&roots, &trust, &kinds)?;
    let handlers = ryeos_engine::test_support::load_live_handler_registry();
    let composers = ryeos_engine::composers::ComposerRegistry::from_kinds(&kinds, &handlers)?;
    let tagged = roots
        .iter()
        .map(|root| {
            (
                root.clone(),
                ryeos_engine::resolution::TrustClass::TrustedBundle,
            )
        })
        .collect::<Vec<_>>();
    let protocols = ryeos_engine::protocols::ProtocolRegistry::load_base(&tagged, &trust)?;
    let validators = ryeos_engine::effective_validators::EffectiveValidatorRegistry::from_kinds(
        &kinds,
        &handlers,
        state.isolation.clone(),
        &roots,
    )?;
    let registered = ["core", "standard", "qualification-fixture"]
        .into_iter()
        .zip(roots.iter())
        .map(
            |(name, root)| ryeos_engine::item_resolution::RegisteredBundleRoot {
                name: name.into(),
                canonical_root: root.clone(),
            },
        )
        .collect();
    state.engine = Arc::new(
        ryeos_engine::engine::Engine::new(
            kinds,
            ryeos_engine::parsers::ParserDispatcher::new(parsers, handlers),
            roots,
        )
        .with_trust_store(trust.clone())
        .with_node_trust_store(trust)
        .with_composers(composers)
        .with_protocols(protocols)
        .with_effective_validators(validators)
        .with_registered_bundle_roots(registered),
    );
    let resolution = state.engine.with_checked_bundle_generation(|generation| {
        resolve_consumer_definition_in_generation(generation, AUX_VERIFIER, "tool")
    })?;
    let consumer = crate::external_content_admission::consumer_authority(
        &resolution,
        &SubjectResolutionAuthority::Projectless,
    )?;
    let grant = crate::operator_authority::admitted_operator_authority_digest(
        &state,
        operator.fingerprint(),
    )?;
    let binding = ExternalContentBinding::active(
        manifest_hash.clone(),
        EXTERNAL_CONTENT_MANIFEST_KIND.into(),
        consumer,
        state.identity.fingerprint().into(),
        operator.fingerprint().into(),
        grant,
    )?;
    let guard = authority.acquire_exclusive_guard(true)?;
    let binding_hash = cas.store_object(&binding.to_value()?)?;
    let node_signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
        db.ensure_current_external_content_binding_epoch(&guard)?;
        db.advance_generic_head_ref(
            EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
            &binding.binding_subject_id,
            &binding_hash,
            None,
            &node_signer,
            &guard,
        )
    })?;
    drop(guard);
    Ok(Fixture {
        directory,
        state,
        context,
        manifest_hash,
    })
}

#[test]
fn signed_auxiliary_fixture_reconstructs_its_current_verifier() -> Result<()> {
    let fixture = fixture()?;
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let current = resolve_current_bundle_verifier_identity_against_admitted(
        &fixture.state,
        &authority,
        &guard,
        &fixture.context,
        AUX_VERIFIER,
        &json!({}),
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(None),
            logical_project_root: None,
            binding_subject_authority: None,
            sealed_request: None,
            project_context_resolver: None,
            pinned_admission: None,
        },
        None,
    )?;
    assert_eq!(
        current.realizations.iter().next().unwrap().manifest_hash,
        fixture.manifest_hash
    );
    assert!(matches!(
        current.artifact_identity,
        AdmittedLaunchArtifactIdentity::DirectItemExecutor { .. }
    ));
    Ok(())
}

#[test]
fn published_auxiliary_fixture_passes_current_qualification_consumption() -> Result<()> {
    let fixture = fixture()?;
    let (hash, evidence) = seed_auxiliary_qualification(&fixture)?;
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let limits = fixture
        .state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let proof = load_current_qualification(
        &fixture.state,
        &authority,
        &guard,
        limits,
        &fixture.context.fingerprint,
        &hash,
    )?;
    assert_eq!(proof.evidence, evidence);
    let current = resolve_current_bundle_verifier_identity_for_evidence(
        &fixture.state,
        &authority,
        &guard,
        limits,
        &fixture.context,
        AUX_VERIFIER,
        &json!({}),
        &evidence,
        None,
    )?;
    evidence.validate_current_policy(
        &resolve_current_bundle_qualification_policy(&fixture.state, AUX_POLICY)?,
        &current.effective_definition_digest,
        &["fixture_auxiliary".into()],
    )?;
    evidence.validate_current_artifact(&current.artifact_identity)?;
    execution_evidence::verify_current(
        &fixture.state,
        &authority,
        &guard,
        &fixture.context,
        &evidence,
        &current,
        None,
    )?;
    Ok(())
}

#[derive(Clone)]
struct SnapshotLease {
    snapshot: String,
    path: std::path::PathBuf,
    engine: Arc<ryeos_engine::engine::Engine>,
    materialization: ryeos_state::PinnedProjectMaterialization,
    lifeline: Arc<crate::temp_dir_guard::TempDirGuard>,
    resolutions: Arc<std::sync::atomic::AtomicUsize>,
}

impl QualificationProjectContextLease for SnapshotLease {
    fn snapshot_hash(&self) -> &str {
        &self.snapshot
    }
    fn original_path(&self) -> &Path {
        &self.path
    }
    fn effective_path(&self) -> &Path {
        &self.path
    }
    fn request_engine(&self) -> &Arc<ryeos_engine::engine::Engine> {
        &self.engine
    }
    fn pinned_materialization(&self) -> &ryeos_state::PinnedProjectMaterialization {
        &self.materialization
    }
    fn workspace_lifeline(&self) -> &Arc<crate::temp_dir_guard::TempDirGuard> {
        &self.lifeline
    }
}

impl QualificationProjectContextResolver for SnapshotLease {
    fn resolve_read_only_snapshot(
        &self,
        snapshot_hash: &str,
        display_path: &Path,
        _: &str,
    ) -> Result<Box<dyn QualificationProjectContextLease>> {
        anyhow::ensure!(
            snapshot_hash == self.snapshot && display_path == self.path,
            "fixture resolver requires its exact retained snapshot"
        );
        self.materialization.ensure_path_binding()?;
        self.resolutions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::new(self.clone()))
    }
}

fn pinned_snapshot(fixture: &Fixture) -> Result<SnapshotLease> {
    use ryeos_state::project_sync::{PROJECT_SNAPSHOT_CONFIG_RELATIVE, ProjectSyncScope};
    let path = fixture.directory.path().join("pinned-project");
    let policy_path = path.join(PROJECT_SNAPSHOT_CONFIG_RELATIVE);
    fs::create_dir_all(policy_path.parent().unwrap())?;
    let bytes = b"schema: 1\nexclusions: []\n";
    fs::write(&policy_path, bytes)?;
    let policy = ryeos_state::project_sync::capture_snapshot_policy(
        &path,
        &fixture.state.ignore_matcher,
        ProjectSyncScope::FullProject,
    )?;
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let file = ProjectFile {
        blob_hash: cas.store_blob(bytes)?,
        size: bytes.len() as u64,
        normalized_mode: 0o644,
    };
    let tree = ProjectTree {
        files: [(
            PROJECT_SNAPSHOT_CONFIG_RELATIVE.into(),
            cas.store_object(&file.to_value())?,
        )]
        .into(),
    };
    let snapshot = ProjectSnapshot {
        project_tree_hash: cas.store_object(&tree.to_value())?,
        effective_policy_hash: cas.store_object(&policy.to_value())?,
        parent_hashes: vec![],
        message: None,
        created_at: "2026-09-29T00:00:00Z".into(),
        source: "qualification source acceptance fixture".into(),
    };
    let snapshot = cas.store_object(&snapshot.to_value())?;
    let materialization =
        ryeos_state::PinnedProjectMaterialization::verify(&authority, &guard, &snapshot, &path)?;
    Ok(SnapshotLease {
        snapshot,
        path: path.clone(),
        engine: Arc::clone(&fixture.state.engine),
        materialization,
        lifeline: Arc::new(crate::temp_dir_guard::TempDirGuard::new(path)),
        resolutions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
}

struct PinnedFixture {
    snapshot: SnapshotLease,
    plan: PlanContext,
    binding: crate::thread_lifecycle::AdmittedProjectBinding,
    request: launch::ProductQualificationLaunchRequest,
}

fn pinned_fixture(fixture: &Fixture) -> Result<PinnedFixture> {
    let snapshot = pinned_snapshot(fixture)?;
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let bytes = b"distinct subject input";
    let manifest = cas.store_object(&json!({
        "kind": EXTERNAL_CONTENT_MANIFEST_KIND, "schema": EXTERNAL_CONTENT_TREE_SCHEMA,
        "entries": [{"path":"subject", "kind":"file", "mode":420, "blob_hash":cas.store_blob(bytes)?, "size":bytes.len()}],
        "entry_count":1, "total_bytes":bytes.len(),
    }))?;
    let outer = relationship(
        "subject_to_downstream",
        "subject",
        "subject",
        Some(TARGET_POLICY),
    );
    let (witness_hash, _) = publish_fixture_product(
        fixture,
        "subject",
        &manifest,
        bytes.len() as u64,
        composition::ProductRelationships {
            schema: composition::PRODUCT_RELATIONSHIPS_SCHEMA.into(),
            relationships: vec![outer],
        },
    )?;
    let plan = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: fixture.context.fingerprint.clone(),
            scopes: fixture.context.scopes.clone(),
        }),
        project_context: ProjectContext::SnapshotHash {
            hash: snapshot.snapshot.clone(),
        },
        subject_resolution_authority: SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: snapshot.snapshot.clone(),
        },
        current_site_id: fixture.state.threads.site_id().into(),
        origin_site_id: fixture.state.threads.site_id().into(),
        execution_hints: ExecutionHints::default(),
        scheduled_fire: None,
        validate_only: false,
    };
    let project_authority = ExecutionProjectAuthority::pinned(
        "qualification-source-fixture".into(),
        Some(snapshot.path.clone()),
        snapshot.snapshot.clone(),
        PinnedProjectRealization::ReadOnly,
        EnvironmentAuthority::None,
        vec![],
    )?;
    let binding = crate::thread_lifecycle::AdmittedProjectBinding::from_qualification_snapshot(
        &snapshot.engine,
        &plan,
        project_authority,
        &snapshot.path,
        &snapshot.path,
        &snapshot.materialization,
        Arc::clone(&snapshot.lifeline),
    )?;
    let (qualification_hash, qualified) = seed_auxiliary_qualification_in_context(
        fixture,
        Some(&QualificationPinnedAdmissionContext {
            plan_context: &plan,
            request_engine: &snapshot.engine,
            project_binding: &binding,
            project_root: &snapshot.path,
            project_authority: binding.exact_authority(),
        }),
    )?;
    let request = launch::ProductQualificationLaunchRequest {
        consumer_invocation: None,
        launch_id: format!("L-{}", "2".repeat(32)),
        witness_hash,
        witness_source: ProductWitnessSource::LocalCapture {},
        relationship_name: "subject_to_downstream".into(),
        project_context: Some(launch::ProductQualificationProjectContext {
            snapshot_hash: snapshot.snapshot.clone(),
        }),
        auxiliary_product_selections: vec![ProductSelectionInput {
            target: ProductSelectionTarget::Root {},
            selection: ProductSelection {
                declaration_id: "auxiliary".into(),
                witness_hash: qualified.product_witness_hash,
                witness_source: ProductWitnessSource::LocalCapture {},
                qualification_hash: Some(qualification_hash),
            },
        }],
    };
    Ok(PinnedFixture {
        snapshot,
        plan,
        binding,
        request,
    })
}

fn preflight_pinned(
    fixture: &Fixture,
    pinned: &PinnedFixture,
    prepared: &launch::PreparedProductQualificationLaunch,
) -> Result<crate::thread_lifecycle::RootExecutionAdmission> {
    Ok(crate::thread_lifecycle::preflight_root_execution(
        crate::thread_lifecycle::ResolveRootExecutionParams {
            engine: &pinned.snapshot.engine,
            plan_context: pinned.plan.clone(),
            project_binding: pinned.binding.clone(),
            node_history_policy: fixture.state.node_history_policy()?,
            item_ref: &prepared.verifier_ref,
            ref_bindings: BTreeMap::new(),
            product_selections: prepared.product_selections.clone(),
            launch_mode: "wait",
            parameters: prepared.verifier_parameters.clone(),
            usage_subject: None,
            usage_subject_asserted_by: None,
            creates_chain_root: true,
        },
    )?
    .root_admission)
}

/// Install the ordinary active content binding after real selection admission.
/// Only historical node testimony is seeded; source/selection/proof, manifest,
/// consumer identity and current qualification are authenticated here.
fn bind_pinned_inputs(fixture: &Fixture, pinned: &PinnedFixture) -> Result<()> {
    let prepared =
        launch::prepare_after_reservation(&fixture.state, &fixture.context, &pinned.request)?;
    let admission = preflight_pinned(fixture, pinned, &prepared)?;
    let mut resolution = admission.resolution_output().clone();
    let selectors = prepared
        .product_selections
        .iter()
        .map(|input| input.selection.clone())
        .collect::<Vec<_>>();
    let retained = crate::operator_external_content::product_composition::select_products_with_project_context_resolver(
        &fixture.state, &fixture.context, &pinned.snapshot.engine,
        &pinned.snapshot.engine.resolution_roots(Some(pinned.snapshot.path.clone())),
        &pinned.plan.subject_resolution_authority, &mut resolution, &selectors, Some(&pinned.snapshot),
    )?;
    assert_eq!(retained.len(), 2);
    assert!(retained.get("subject").unwrap().qualification.is_none());
    assert!(retained.get("auxiliary").unwrap().qualification.is_some());
    assert_ne!(
        retained.get("subject").unwrap().manifest_hash,
        retained.get("auxiliary").unwrap().manifest_hash
    );
    bind_selected_inputs(
        fixture,
        &resolution,
        &pinned.plan.subject_resolution_authority,
        &retained,
    )
}

fn bind_selected_inputs(
    fixture: &Fixture,
    resolution: &ResolutionOutput,
    subject: &SubjectResolutionAuthority,
    retained: &ResolvedExternalProductSelections,
) -> Result<()> {
    let consumer = crate::external_content_admission::consumer_authority(resolution, subject)?;
    let operator =
        crate::operator_authority::require_admitted_operator(&fixture.state, &fixture.context)
            .context("authenticate selected-input binding operator")?;
    let grant =
        crate::operator_authority::admitted_operator_authority_digest(&fixture.state, &operator)
            .context("load selected-input binding operator grant")?;
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let cas = authority.cas_store()?;
    let guard = authority.acquire_exclusive_guard(true)?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&fixture.state.identity);
    for (_, selected) in retained.iter() {
        let binding = ExternalContentBinding::active(
            selected.manifest_hash.clone(),
            selected.manifest_kind.clone(),
            consumer.clone(),
            fixture.state.identity.fingerprint().into(),
            operator.clone(),
            grant.clone(),
        )?;
        let hash = cas.store_object(&binding.to_value()?)?;
        fixture.state.state_store.with_state_db(|db| {
            db.ensure_current_external_content_binding_epoch(&guard)?;
            db.advance_generic_head_ref(
                EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE,
                &binding.binding_subject_id,
                &hash,
                None,
                &signer,
                &guard,
            )
        })?;
    }
    Ok(())
}

fn finalize_pinned(
    fixture: &Fixture,
    pinned: &PinnedFixture,
    request: &launch::ProductQualificationLaunchRequest,
) -> Result<launch::PreparedProductQualificationLaunch> {
    let mut prepared = launch::prepare_after_reservation(&fixture.state, &fixture.context, request)
        .context("prepare complete qualification launch")?;
    let admission = preflight_pinned(fixture, pinned, &prepared)
        .context("preflight complete qualification launch")?;
    finalize_pinned_qualification_launch(
        &fixture.state,
        &fixture.context,
        &mut prepared,
        &admission,
        &pinned.snapshot.engine,
        &pinned.plan,
        &pinned.binding,
        &pinned.snapshot.path,
        pinned.binding.exact_authority(),
        &pinned.snapshot,
    )
    .context("finalize complete pinned qualification launch")?;
    Ok(prepared)
}

fn seal_complete_launch(
    fixture: &Fixture,
    pinned: &PinnedFixture,
    prepared: &launch::PreparedProductQualificationLaunch,
) -> Result<crate::thread_lifecycle::SealedRootExecutionRequest> {
    use ryeos_state::external_content::qualification_purpose::{
        QUALIFICATION_LAUNCH_PURPOSE_SCHEMA, QualificationLaunchPurpose, QualificationSubject,
    };
    let admission = preflight_pinned(fixture, pinned, prepared)
        .context("preflight complete launch for sealing")?;
    let mut resolution = admission.resolution_output().clone();
    let selectors = prepared
        .product_selections
        .iter()
        .map(|input| input.selection.clone())
        .collect::<Vec<_>>();
    let retained = crate::operator_external_content::product_composition::select_products_with_project_context_resolver(
        &fixture.state, &fixture.context, &pinned.snapshot.engine,
        &pinned.snapshot.engine.resolution_roots(Some(pinned.snapshot.path.clone())),
        &pinned.plan.subject_resolution_authority, &mut resolution, &selectors, Some(&pinned.snapshot),
    ).context("select complete inputs for sealing")?;
    let authority = fixture.state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let current = resolve_current_bundle_verifier_identity_against_admitted(
        &fixture.state,
        &authority,
        &guard,
        &fixture.context,
        &prepared.verifier_ref,
        &prepared.verifier_parameters,
        CurrentVerifierContext {
            content: CurrentVerifierContent::Root(Some(&retained)),
            logical_project_root: Some(Path::new(ADMITTED_DIRECT_PROJECT_ROOT)),
            binding_subject_authority: Some(&pinned.plan.subject_resolution_authority),
            sealed_request: None,
            project_context_resolver: Some(&pinned.snapshot),
            pinned_admission: Some(&QualificationPinnedAdmissionContext {
                plan_context: &pinned.plan,
                request_engine: &pinned.snapshot.engine,
                project_binding: &pinned.binding,
                project_root: &pinned.snapshot.path,
                project_authority: pinned.binding.exact_authority(),
            }),
        },
        Some(admission.resolution_output()),
    )
    .context("reconstruct complete current verifier for sealing")?;
    assert_eq!(
        prepared.verifier_admitted_definition_digest.as_deref(),
        Some(current.admitted_definition_digest.as_str())
    );
    assert_eq!(
        prepared.verifier_realized_definition_digest.as_deref(),
        Some(current.effective_definition_digest.as_str())
    );
    let finalized =
        finalized_current(fixture, &current).context("relock complete verifier for sealing")?;
    let consumer_content = prepared.consumer_content_identity()?;
    let purpose = QualificationLaunchPurpose {
        consumer_invocation: None,
        schema: QUALIFICATION_LAUNCH_PURPOSE_SCHEMA.into(),
        launch_id: prepared.launch_id.clone(),
        owner_fingerprint: prepared.owner_fingerprint.clone(),
        subject: QualificationSubject::CapturedProduct {
            product_witness_hash: prepared.product_witness_hash.clone(),
            witness_source: prepared.witness_source.clone(),
            relationship_name: prepared.relationship.name.clone(),
        },
        policy_source: prepared.policy_source.clone(),
        consumer_definitions: consumer_content
            .as_ref()
            .map(|content| content.definitions.clone()),
        consumer_content,
        producer_recipe_sources: resolve_current_bundle_producer_recipes_for_policy(
            &fixture.state,
            &prepared.policy_source,
            &prepared.subject_manifest_hash,
        )?,
        remote_verifier_sources: BTreeMap::new(),
        subject_declaration_id: prepared.policy_source.policy.subject_declaration_id.clone(),
        subject_manifest_hash: prepared.subject_manifest_hash.clone(),
        required_claims: prepared.relationship.qualification.required_claims.clone(),
        admitted_parameters_digest: prepared.policy_source.policy.admitted_parameters_digest()?,
        verifier_ref: prepared.verifier_ref.clone(),
        verifier_effective_definition_digest: current.admitted_definition_digest,
        verifier_realized_definition_digest: current.effective_definition_digest,
    };
    let admission = admission
        .for_qualification(purpose)
        .context("attach complete two-input qualification purpose")?;
    let request = admission
        .execution_request(
            crate::thread_lifecycle::RootExecutionRoute::RootExecutorChain,
            "wait".into(),
            prepared.verifier_parameters.clone(),
        )
        .context("resolve complete execution request")?;
    let plan = crate::thread_lifecycle::prepare_bundle_item_plan_for_qualification(
        &pinned.snapshot.engine,
        &request,
        &fixture.state.isolation,
        &finalized,
        Some(Path::new(ADMITTED_DIRECT_PROJECT_ROOT)),
    )
    .context("prepare complete direct plan")?;
    let sealed = crate::thread_lifecycle::SealedRootExecutionRequest::capture_finalized(
        &request,
        plan.runtime_ref()?.into(),
        &finalized,
        Some(&fixture.context),
    )
    .context("seal finalized complete two-input qualification request")?;
    assert_eq!(sealed.product_selections(), &prepared.product_selections);
    assert!(!sealed.requires_process_scope_for_qualification()?);
    let (_, retained_after_sealing) = retained_verifier_root_selections(
        sealed.product_selections(),
        sealed.admitted_effective_resolution()?,
        &prepared.policy_source.policy.subject_declaration_id,
    )?;
    assert_eq!(retained_after_sealing, Some(retained));
    Ok(sealed)
}

#[test]
fn pinned_finalizer_admits_subject_and_current_qualified_auxiliary() -> Result<()> {
    let fixture = fixture()?;
    let pinned = pinned_fixture(&fixture).context("construct pinned complete-input fixture")?;
    bind_pinned_inputs(&fixture, &pinned).context("bind complete inputs")?;
    let prepared = finalize_pinned(&fixture, &pinned, &pinned.request)?;
    let sealed = seal_complete_launch(&fixture, &pinned, &prepared)?;
    assert_eq!(
        sealed.qualification_purpose().unwrap().launch_id,
        prepared.launch_id
    );
    assert!(
        pinned
            .snapshot
            .resolutions
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 2,
        "current auxiliary qualification must reconstruct its own pinned capsule through the supplied resolver"
    );
    assert_eq!(prepared.product_selections.len(), 2);
    let subject = prepared
        .product_selections
        .iter()
        .find(|input| input.selection.declaration_id == "subject")
        .unwrap();
    assert_eq!(subject.selection.witness_hash, pinned.request.witness_hash);
    assert!(subject.selection.qualification_hash.is_none());
    assert_eq!(
        prepared.product_selections[0],
        pinned.request.auxiliary_product_selections[0]
    );
    assert!(
        prepared
            .verifier_admitted_definition_digest
            .as_ref()
            .is_some_and(|d| lillux::valid_hash(d))
    );
    assert!(
        prepared
            .verifier_realized_definition_digest
            .as_ref()
            .is_some_and(|d| lillux::valid_hash(d))
    );
    Ok(())
}

#[test]
fn pinned_finalizer_refuses_missing_unknown_swapped_and_stale_auxiliary_proofs() -> Result<()> {
    let fixture = fixture()?;
    let pinned = pinned_fixture(&fixture).context("construct negative pinned fixture")?;
    bind_pinned_inputs(&fixture, &pinned).context("bind negative fixture complete inputs")?;
    finalize_pinned(&fixture, &pinned, &pinned.request)?;
    let mut missing = pinned.request.clone();
    missing.auxiliary_product_selections.clear();
    let mut unknown = pinned.request.clone();
    unknown.auxiliary_product_selections[0]
        .selection
        .declaration_id = "unknown".into();
    let mut unqualified = pinned.request.clone();
    unqualified.auxiliary_product_selections[0]
        .selection
        .qualification_hash = None;
    let mut swapped = pinned.request.clone();
    swapped.auxiliary_product_selections[0]
        .selection
        .qualification_hash = Some(pinned.request.witness_hash.clone());
    let mut subject_swap = pinned.request.clone();
    subject_swap.auxiliary_product_selections[0]
        .selection
        .witness_hash = pinned.request.witness_hash.clone();
    for (label, request, reason) in [
        ("missing slot", missing, "exactly cover"),
        ("unknown slot", unknown, "exactly cover"),
        ("missing proof", unqualified, "qualification proof"),
        ("swapped proof", swapped, "qualification"),
        ("swapped witness", subject_swap, "product"),
    ] {
        let error = finalize_pinned(&fixture, &pinned, &request)
            .err()
            .with_context(|| format!("{label} unexpectedly admitted"))?;
        assert!(format!("{error:#}").contains(reason), "{label}: {error:#}");
    }
    // Current signed policy drift invalidates the previously accepted proof;
    // this changes the authenticated source, never the validator or evidence.
    let policy_path = fixture
        .directory
        .path()
        .join("fixture-bundle/.ai/config/qualification-fixture/auxiliary-pinned.yaml");
    let source = fs::read_to_string(&policy_path)?;
    let source = source
        .lines()
        .filter(|line| !line.starts_with("#"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("version: '1.0.0'", "version: '2.0.0'");
    write_signed(
        &policy_path,
        &format!("{source}\n"),
        &SigningKey::from_bytes(&[53; 32]),
    )?;
    let error = finalize_pinned(&fixture, &pinned, &pinned.request)
        .err()
        .context("stale signed auxiliary policy unexpectedly admitted")?;
    assert!(
        format!("{error:#}").contains("policy") || format!("{error:#}").contains("generation"),
        "{error:#}"
    );
    Ok(())
}
