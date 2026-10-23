//! Two-daemon end-to-end proof that a remote qualification testimony is
//! accepted by a hosted node only under that node's own signed policy.
//!
//! ORIGIN publishes a product and pushes it to HOSTED. The origin operator
//! then signs a testimony over a qualification attestation that the origin's
//! node key signed for that product, and submits it to HOSTED through the
//! authenticated accept route, exactly as the origin's configured-operator
//! client does: the operator key signs the request and ORIGIN's node key
//! co-signs it as the forwarding authority.
//!
//! HOSTED carries a signed Bundle holding the qualification policy the
//! attestation was made under and several acceptance policies. The
//! attestation's policy source is resolved from HOSTED's own stopped engine,
//! so the evidence names the exact policy identity HOSTED will resolve again
//! at acceptance. No verifier runs anywhere in this test: the origin's export
//! verb re-verifies a qualification against retained verifier threads, which
//! a seeded attestation cannot satisfy, so the origin side is only asserted
//! to refuse a qualification it does not hold.

use ryeosd::test_support as common;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_app::test_support::TestStateStore;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_state::LargeObjectStore;
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, publish_product_witness,
};
use ryeos_state::external_content::products::qualification::{
    PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA, PRODUCT_QUALIFICATION_POLICY_SCHEMA,
    PRODUCT_QUALIFICATION_RESULT_SCHEMA, ProductQualificationEvidence,
    ProductQualificationExecutionProof, ProductQualificationPolicy,
    ProductQualificationPolicySource, ProductQualificationProjectorIdentity,
    ProductQualificationResult, ProductQualificationVerifier, VerifierProcessSettlementAuthority,
};
use ryeos_state::external_content::products::qualification_publication::PRODUCT_QUALIFICATION_HEAD_NAMESPACE;
use ryeos_state::external_content::products::qualification_testimony::{
    REMOTE_QUALIFICATION_TESTIMONY_CLAIM, REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE,
    REMOTE_QUALIFICATION_TESTIMONY_POLICY, RemoteQualificationTestimonyBody,
};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use ryeos_state::external_content::products::{
    PRODUCT_DECLARATIONS_SCHEMA, ProductBounds, ProductCaptureEvidence, ProductDeclaration,
    ProductDeclarations, ProductProducerAdmission, ProductShape, ProductStorage,
};
use ryeos_state::object_closure::ObjectClosureLimits;
use ryeos_state::objects::{
    AdmittedLaunchArtifactIdentity, Attestation, DirectExecutableIdentity,
    DirectRootSourceIdentity, DirectRuntimeIdentity, DirectRuntimeSourceSpace,
    canonical_value_digest,
};
use serde_json::{Value, json};

use common::DaemonHarness;
use common::fast_fixture::{FastFixture, register_config_fixture_bundle};

/// Byte bound every push request carries.
const MAXIMUM_BYTES: u64 = 8 * 1024 * 1024;
const PAYLOAD_BYTES: usize = 64 * 1024 + 17;
const PRODUCT_ROOT: &str = "T-00000000-0000-0000-0000-000000000001";
const PRODUCT_TERMINAL: &str = "T-00000000-0000-0000-0000-000000000002";
const VERIFIER_ROOT: &str = "T-00000000-0000-0000-0000-000000000011";
const VERIFIER_TERMINAL: &str = "T-00000000-0000-0000-0000-000000000012";
const HOSTED_REMOTE: &str = "hosted";
const ORIGIN_REMOTE: &str = "origin";

const ACCEPT_PATH: &str = "/external-content/accept-qualification-testimony";
const ACCEPT_SCOPE: &str = "ryeos.execute.service.external-content/accept-qualification-testimony";

/// Policy Configs carried by HOSTED's fixture Bundle. All of them are signed
/// by the fixture publisher, which the node trusts as a Bundle signer.
const FIXTURE_BUNDLE: &str = "fixture-testimony-policies";
const QUALIFICATION_POLICY_REF: &str = "config:testimony/qualification-policy";
const ACCEPT_POLICY_REF: &str = "config:testimony/acceptance-policy";
const UNLISTED_POLICY_REF: &str = "config:testimony/acceptance-policy-unlisted";
const STALE_POLICY_REF: &str = "config:testimony/acceptance-policy-stale";

const CLAIM: &str = "command_probe";

/// Capabilities granted to the origin operator on HOSTED.
const OPERATOR_SCOPES: &[&str] = &[
    "ryeos.execute.service.external-content/product-offer",
    "ryeos.execute.service.external-content/product-accept",
    "ryeos.execute.service.objects/stage-chunk",
    ACCEPT_SCOPE,
];

/// What the origin NODE's grant on HOSTED carries: the operator's
/// capabilities plus the authority to co-sign forwarded operator requests.
const ORIGIN_NODE_SCOPES: &[&str] = &[
    "ryeos.execute.service.external-content/product-offer",
    "ryeos.execute.service.external-content/product-accept",
    "ryeos.execute.service.objects/stage-chunk",
    ACCEPT_SCOPE,
    ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE,
];

fn hosted_node_key() -> SigningKey {
    SigningKey::from_bytes(&[71_u8; 32])
}

fn hosted_operator_key() -> SigningKey {
    SigningKey::from_bytes(&[72_u8; 32])
}

/// A node key that is not ORIGIN's, standing for a forger.
fn rogue_node_key() -> SigningKey {
    SigningKey::from_bytes(&[75_u8; 32])
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn site_of(node_key: &SigningKey) -> String {
    format!("site:{}", fingerprint(node_key))
}

fn operator_principal() -> String {
    format!(
        "fp:{}",
        fingerprint(&common::fast_fixture::user_signing_key())
    )
}

fn daemon_identity(state_path: &Path) -> Result<NodeIdentity> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    NodeIdentity::load(&identity_path)
        .with_context(|| format!("load daemon identity {}", identity_path.display()))
}

/// Open a stopped daemon's node store through the native test gate. The owner
/// stays on the opening thread, so callers hold it only across synchronous
/// code.
fn open_daemon_state(state_path: &Path) -> Result<TestStateStore> {
    let identity = daemon_identity(state_path)?;
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    TestStateStore::open(state_path, signer, Arc::new(head_trust))
}

fn remote_config(
    name: &str,
    url: String,
    node_key: &SigningKey,
    site_id: &str,
) -> ryeos_api::remote::config::RemoteConfig {
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(node_key.verifying_key().as_bytes());
    ryeos_api::remote::config::RemoteConfig {
        name: name.to_owned(),
        url,
        principal_id: format!("fp:{}", fingerprint(node_key)),
        signing_key: format!("ed25519:{public_key}"),
        site_id: site_id.to_owned(),
        vault_fingerprint: format!("sha256:{name}-fixture-vault"),
        ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
        project_bindings: HashMap::new(),
    }
}

fn install_single_remote(
    state_path: &Path,
    remote: ryeos_api::remote::config::RemoteConfig,
) -> Result<()> {
    ryeos_api::remote::config::save_remotes(
        state_path,
        &HashMap::from([(remote.name.clone(), remote)]),
    )
}

fn authorized_keys_dir(state_path: &Path) -> std::path::PathBuf {
    state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/auth/authorized_keys")
}

fn scope_strings(scopes: &[&str]) -> Vec<String> {
    scopes.iter().map(|scope| (*scope).to_owned()).collect()
}

/// Admit `remote_node` on the daemon at `state_path` as a remote node of
/// `remote_site_id`.
fn authorize_remote_node(
    state_path: &Path,
    local: &FastFixture,
    remote_node: &SigningKey,
    remote_site_id: &str,
    scopes: &[&str],
) -> Result<()> {
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(remote_node.verifying_key().as_bytes());
    ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
        &authorized_keys_dir(state_path),
        &fingerprint(remote_node),
        &public_key,
        &scope_strings(scopes),
        "testimony-transport-peer",
        &local.node_fp(),
        &lillux::time::iso8601_now(),
        remote_site_id,
        &local.node,
    )?;
    Ok(())
}

/// Admit `remote_operator` on the daemon at `state_path` as a remote operator
/// bound to `remote_site_id`.
fn authorize_remote_operator(
    state_path: &Path,
    local: &FastFixture,
    remote_operator: &SigningKey,
    remote_site_id: &str,
    scopes: &[&str],
) -> Result<()> {
    let public_key = base64::engine::general_purpose::STANDARD
        .encode(remote_operator.verifying_key().as_bytes());
    let identity = daemon_identity(state_path)?;
    ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes(
        &authorized_keys_dir(state_path),
        &fingerprint(remote_operator),
        &public_key,
        &scope_strings(scopes),
        "testimony-transport-operator",
        &local.node_fp(),
        &lillux::time::iso8601_now(),
        &identity,
        ryeos_node_runtime::identity::WildcardPolicy::Reject,
        false,
        Some(remote_site_id),
        false,
    )?;
    Ok(())
}

/// A state-layer signer over a bare key, so the operator key signs a
/// testimony without a key file on disk.
struct KeySigner {
    key: SigningKey,
    fingerprint: String,
}

impl KeySigner {
    fn new(key: SigningKey) -> Self {
        let fingerprint = fingerprint(&key);
        Self { key, fingerprint }
    }
}

impl ryeos_state::Signer for KeySigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        lillux::crypto::Signer::sign(&self.key, data)
            .to_bytes()
            .to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
        self.key.verifying_key()
    }
}

/// What ORIGIN published.
struct Published {
    witness_hash: String,
    admission_hash: String,
    manifest_hash: String,
    coordinate: ProductCaptureCoordinate,
}

/// Ingest a small payload into the stopped daemon's large-object store, then
/// publish its manifest, a node-signed witness owned by `owner_principal`, and
/// the node's admission of the witness. Synchronous so the store owner never
/// crosses an await.
fn publish_product(state_path: &Path, scratch: &Path, owner_principal: &str) -> Result<Published> {
    let mut payload = vec![0_u8; PAYLOAD_BYTES];
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte = u8::try_from((index * 31 + 7) % 251)?;
    }
    let identity = daemon_identity(state_path)?;
    let signer = NodeIdentitySigner::from_identity(&identity);
    let store = open_daemon_state(state_path)?;
    let authority = store.pinned_state_authority()?;

    let large = LargeObjectStore::open_or_create_under(authority.runtime_directory())?;
    std::fs::create_dir_all(scratch)?;
    std::fs::write(scratch.join("payload.bin"), &payload)?;
    let directory = lillux::PinnedDirectory::open(scratch)?.context("open payload directory")?;
    let listed = directory
        .entries_no_follow()?
        .into_iter()
        .find(|entry| entry.name == "payload.bin")
        .context("payload file is not listed")?;
    let file = directory
        .open_regular(std::ffi::OsStr::new("payload.bin"), false)?
        .context("open payload file")?;
    let ingested = large.ingest_open_regular(
        file,
        ryeos_state::PinnedLargeObjectSourceIdentity {
            containing_device: listed.containing_device,
            inode: listed.inode,
            size: u64::try_from(payload.len())?,
        },
        "payload.bin",
        None,
    )?;
    let entry = ryeos_state::objects::ExternalLargeContentManifestEntry {
        path: "content".to_owned(),
        kind: ryeos_state::objects::ExternalContentManifestEntryKind::File,
        mode: Some(493),
        blob_hash: None,
        file_sha256: Some(ingested.file_sha256.clone()),
        size: Some(ingested.size),
        chunk_size: Some(ingested.chunk_size),
        chunk_hashes: ingested.chunk_hashes.clone(),
        target: None,
    };

    let limits = ObjectClosureLimits::default();
    let guard = authority.acquire_exclusive_guard(true)?;
    let cas = authority.cas_store()?;
    let manifest_hash = cas.store_object(&json!({
        "kind": ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND,
        "schema": ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
        "entries": [serde_json::to_value(&entry)?],
        "entry_count": 1,
        "total_bytes": ingested.size,
    }))?;
    let declaration = ProductDeclaration {
        name: "runtime".to_owned(),
        source: ryeos_state::external_content::products::ProductSource::RetainedProject {},
        path: "products/runtime".to_owned(),
        shape: ProductShape::Tree,
        storage: ProductStorage::LargeContent,
        required: true,
        bounds: ProductBounds {
            maximum_entries: 16,
            maximum_depth: 4,
            maximum_file_bytes: 4 * 1024 * 1024,
            maximum_total_bytes: 8 * 1024 * 1024,
        },
        expected_manifest_hash: None,
    };
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.to_owned(),
        output_roots: Vec::new(),
        products: vec![declaration.clone()],
    };
    let producer = || ProductProducerAdmission {
        canonical_ref: "graph:test/build".to_owned(),
        effective_definition_digest: "1".repeat(64),
        exact_program_hash: "2".repeat(64),
        producer_project_snapshot_hash: "3".repeat(64),
        launch_authority_digest: "4".repeat(64),
        admitted_parameters_digest: "5".repeat(64),
    };
    let evidence = ProductCaptureEvidence {
        schema: ryeos_state::external_content::products::PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: owner_principal.to_owned(),
        chain_root_id: PRODUCT_ROOT.to_owned(),
        thread_id: PRODUCT_TERMINAL.to_owned(),
        // Historical coordinates are node testimony; no launch capsule is
        // installed behind them.
        admitted_launch_capsule_hash: "a".repeat(64),
        root_producer: producer(),
        producer: producer(),
        result_project_snapshot_hash: "b".repeat(64),
        workspace_output_capture_hash: None,
        producer_partition_identity: None,
        recipe_binding: "product_recipe".to_owned(),
        recipe_purpose:
            ryeos_state::external_content::products::ProductRecipePurpose::GeneralProductV1,
        recipe_ref: "config:test/products".to_owned(),
        recipe_raw_content_digest: "c".repeat(64),
        declarations_hash: declarations.content_hash()?,
        declarations,
        relationships:
            ryeos_state::external_content::products::composition::ProductRelationships::empty(),
        declaration,
        capture_policy_digest: "d".repeat(64),
        manifest_hash: manifest_hash.clone(),
        manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
        entry_count: 1,
        total_bytes: ingested.size,
    };
    let coordinate = ProductCaptureCoordinate::from_evidence(&evidence)?;
    let attestation = evidence.sign_attestation(&signer, lillux::time::iso8601_now())?;
    let witness_hash = publish_product_witness(
        &authority,
        &coordinate,
        &attestation,
        limits,
        &signer,
        &guard,
    )?
    .witness
    .attestation_hash;
    drop(guard);

    let admission_hash = {
        let guard = authority.acquire_shared_guard()?;
        let mut request = ryeos_state::AdmissionRequest::accepted(
            witness_hash.clone(),
            ryeos_state::admission::LOCAL_ADMISSION_POLICY,
        );
        request.limits = limits;
        store
            .with_state_db(|db| {
                ryeos_state::admit_root(db, &request, &signer, identity.verifying_key(), &guard)
            })?
            .attestation_hash
    };
    Ok(Published {
        witness_hash,
        admission_hash,
        manifest_hash,
        coordinate,
    })
}

// ----- the qualification fixture ------------------------------------------

fn qualification_policy() -> ProductQualificationPolicy {
    ProductQualificationPolicy {
        schema: PRODUCT_QUALIFICATION_POLICY_SCHEMA.into(),
        verifier_ref: "tool:fixtures/qualify_runtime".into(),
        subject_declaration_id: "runtime".into(),
        allowed_claims: vec![CLAIM.into(), "extension_probe".into()],
        minimum_verifier_process_settlement: VerifierProcessSettlementAuthority::ScopeEmpty,
        verifier_parameters: json!({"scope": "bounded_fixture"}),
        consumer_execution_context: None,
        producer_scenarios: BTreeMap::new(),
    }
}

fn verifier_artifact_identity() -> AdmittedLaunchArtifactIdentity {
    AdmittedLaunchArtifactIdentity::DirectItemExecutor {
        executor_ref: "runtime:fixtures/qualified".into(),
        root_subject_source_content_digest: "1".repeat(64),
        root_subject_signer_fingerprint: Some("2".repeat(64)),
        root_subject_source_identity: DirectRootSourceIdentity::Bundle {
            manifest_hash: "3".repeat(64),
            manifest_signer_fingerprint: "2".repeat(64),
        },
        protocol_ref: "protocol:fixtures/qualified".into(),
        protocol_content_hash: "4".repeat(64),
        protocol_signer_fingerprint: "2".repeat(64),
        execution_plan_hash: "5".repeat(64),
        executable_identity: DirectExecutableIdentity::CapturedContent {
            content_hash: "6".repeat(64),
        },
        runtime_identity: DirectRuntimeIdentity {
            runtime_ref: "runtime:fixtures/qualified".into(),
            runtime_source_space: DirectRuntimeSourceSpace::Bundle,
            runtime_content_hash: "7".repeat(64),
            runtime_signer_fingerprint: "2".repeat(64),
            runtime_bundle_manifest_hash: Some("8".repeat(64)),
            runtime_bundle_signer_fingerprint: Some("2".repeat(64)),
        },
    }
}

fn verifier_definition_digest() -> String {
    "e".repeat(64)
}

fn verifier_artifact_digest() -> Result<String> {
    canonical_value_digest(&serde_json::to_value(verifier_artifact_identity())?)
}

fn execution_proof(
    artifact: &AdmittedLaunchArtifactIdentity,
) -> ProductQualificationExecutionProof {
    let (contract_ref, contract_digest) = match artifact {
        AdmittedLaunchArtifactIdentity::ManagedRuntime {
            runtime_ref,
            runtime_content_hash,
            ..
        } => (runtime_ref, runtime_content_hash),
        AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            protocol_ref,
            protocol_content_hash,
            ..
        } => (protocol_ref, protocol_content_hash),
    };
    ProductQualificationExecutionProof {
        projection_contract_ref: contract_ref.clone(),
        projection_contract_digest: contract_digest.clone(),
        projector: ProductQualificationProjectorIdentity {
            canonical_ref: "handler:fixtures/execution-evidence".into(),
            descriptor_content_digest: "1".repeat(64),
            descriptor_signer_fingerprint: "2".repeat(64),
            binary_content_digest: "3".repeat(64),
            binary_manifest_digest: "4".repeat(64),
            binary_signer_fingerprint: "2".repeat(64),
        },
        participants: Vec::new(),
        subordinate_attempt: None,
    }
}

/// Evidence for a qualification of `published`, made under the policy
/// identity `policy_source`. Nothing here ran: the verifier fields are the
/// shape a real run records.
fn qualification_evidence(
    published: &Published,
    policy_source: ProductQualificationPolicySource,
) -> Result<ProductQualificationEvidence> {
    let result = ProductQualificationResult {
        schema: PRODUCT_QUALIFICATION_RESULT_SCHEMA.into(),
        subject_manifest_hash: published.manifest_hash.clone(),
        claims: vec![CLAIM.into()],
        probe_evidence: json!({"observations": [{"probe": "command", "exit_code": 0}]}),
    };
    let artifact = verifier_artifact_identity();
    let policy = policy_source.policy.clone();
    let evidence = ProductQualificationEvidence {
        schema: PRODUCT_QUALIFICATION_EVIDENCE_SCHEMA.into(),
        product_witness_hash: published.witness_hash.clone(),
        witness_source: ProductWitnessSource::LocalCapture {},
        product_coordinate: published.coordinate.clone(),
        verifier: ProductQualificationVerifier {
            chain_root_id: VERIFIER_ROOT.into(),
            thread_id: VERIFIER_TERMINAL.into(),
            admitted_launch_capsule_hash: "d".repeat(64),
            canonical_ref: policy.verifier_ref.clone(),
            effective_definition_digest: verifier_definition_digest(),
            exact_program_hash: "f".repeat(64),
            admitted_parameters_digest: policy.admitted_parameters_digest()?,
            launch_authority_digest: "1".repeat(64),
            execution_realization_hash: "2".repeat(64),
            artifact_identity: artifact.clone(),
            admitted_project_root: None,
            substrate_identity_hash: "3".repeat(64),
            subject_declaration_id: policy.subject_declaration_id.clone(),
            subject_manifest_hash: result.subject_manifest_hash.clone(),
            terminal_snapshot_hash: "4".repeat(64),
            process_settlement_witness_digest: Some("9".repeat(64)),
            process_settlement_authority: Some(VerifierProcessSettlementAuthority::ScopeEmpty),
            result_digest: result.digest()?,
        },
        policy_source,
        consumer_content: None,
        qualification_worker_transcript_manifest_hash: None,
        verifier_root_selections: None,
        execution_proof: execution_proof(&artifact),
        result,
    };
    evidence.validate()?;
    Ok(evidence)
}

// ----- HOSTED's signed policy Configs --------------------------------------

/// A signed Config body: the metadata every Config carries, plus one policy
/// field.
fn config_source(field: &str, value: Value, signer: &SigningKey) -> Result<String> {
    let mut document = serde_json::Map::new();
    document.insert("category".into(), json!("testimony"));
    document.insert("version".into(), json!("1.0.0"));
    document.insert(
        "description".into(),
        json!("Fixture policy for the remote testimony transport test"),
    );
    document.insert(field.into(), value);
    let body = serde_yaml::to_string(&Value::Object(document))?;
    Ok(lillux::signature::sign_content(&body, signer, "#", None))
}

fn acceptance_policy_value(
    attester_principal: &str,
    origin_site_id: &str,
    definition_digests: &[String],
    artifact_digests: &[String],
) -> Value {
    json!({
        "schema": "ryeos.remote_qualification_acceptance_policy.v1",
        "qualification_policy_ref": QUALIFICATION_POLICY_REF,
        "trusted_attesters": [{
            "principal": attester_principal,
            "origin_site_id": origin_site_id,
        }],
        "required_claims": [CLAIM],
        "allowed_claims": [CLAIM, "extension_probe"],
        "accepted_verifier_definition_digests": definition_digests,
        "accepted_verifier_artifact_digests": artifact_digests,
        "max_testimony_age_seconds": 86400,
    })
}

/// Write the qualification policy and three acceptance policies into the
/// fixture Bundle: one that names the origin operator and the seeded
/// verifier, one that names a different attester, and one that names the
/// origin operator but pins other verifier digests.
fn plant_policies(bundle_root: &Path, publisher: &SigningKey, origin_site_id: &str) -> Result<()> {
    let directory = bundle_root
        .join(ryeos_engine::AI_DIR)
        .join("config/testimony");
    std::fs::create_dir_all(&directory)?;
    let operator = operator_principal();
    let definitions = [verifier_definition_digest()];
    let artifacts = [verifier_artifact_digest()?];
    let other = ["9".repeat(64)];
    let files = [
        (
            "qualification-policy",
            "product_qualification_policy",
            serde_json::to_value(qualification_policy())?,
        ),
        (
            "acceptance-policy",
            "remote_qualification_acceptance_policy",
            acceptance_policy_value(&operator, origin_site_id, &definitions, &artifacts),
        ),
        (
            "acceptance-policy-unlisted",
            "remote_qualification_acceptance_policy",
            acceptance_policy_value(
                &format!("fp:{}", "8".repeat(64)),
                origin_site_id,
                &definitions,
                &artifacts,
            ),
        ),
        (
            "acceptance-policy-stale",
            "remote_qualification_acceptance_policy",
            acceptance_policy_value(&operator, origin_site_id, &other, &other),
        ),
    ];
    for (name, field, value) in files {
        std::fs::write(
            directory.join(format!("{name}.yaml")),
            config_source(field, value, publisher)?,
        )?;
    }
    Ok(())
}

/// The policy identity HOSTED resolves for `policy_ref`, read from an engine
/// built over HOSTED's stopped state the way the node builds its own. The
/// fields are those the node derives for a qualification policy source.
fn resolve_policy_source(
    state_path: &Path,
    policy_ref: &str,
) -> Result<ProductQualificationPolicySource> {
    let config =
        ryeos_node_runtime::config::Config::load(&ryeos_node_runtime::config::ConfigSources {
            app_root: Some(state_path.to_path_buf()),
            ..Default::default()
        })
        .context("load node config")?;
    let isolation = ryeos_node_runtime::engine_init::load_locked_registered_isolation(state_path)
        .context("load retained bundle generation")?;
    let trust = isolation
        .registered_generation_node_trust()
        .context("retained generation omitted node trust")?
        .clone();
    let snapshot = ryeos_node_runtime::node_policy::load_snapshot(
        state_path,
        &trust,
        &ryeos_node_runtime::node_policy::NodePolicyTable::new(),
    )
    .context("load node policy")?;
    let execution = snapshot
        .require::<ryeos_node_runtime::node_policy::sections::execution::NodeExecutionAdmissionPolicy>()?;
    let roots = isolation
        .registered_generation_bundle_roots()
        .context("retained generation omitted bundle roots")?
        .to_vec();
    let engine = ryeos_node_runtime::engine_init::build_registered_engine_for_roots(
        &config,
        &roots,
        None,
        None,
        Arc::clone(&isolation),
        execution,
    )
    .context("build engine over the stopped node")?;
    engine.with_checked_bundle_generation(
        |generation| -> Result<ProductQualificationPolicySource> {
            let resolution = generation.effective_resolution_output(
                ryeos_engine::engine::EffectiveItemRequest {
                    item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(policy_ref)?,
                    expected_kind: Some("config".into()),
                    project_root: None,
                    subject_resolution_authority:
                        ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
                },
            )?;
            let policy = ProductQualificationPolicy::from_value(
                resolution
                    .composed
                    .composed
                    .get("product_qualification_policy")
                    .context("qualification Config has no product_qualification_policy")?,
            )?;
            let source = ProductQualificationPolicySource {
                canonical_ref: resolution.root.resolved_ref.clone(),
                raw_content_digest: resolution.root.raw_content_digest.clone(),
                effective_definition_digest: resolution
                    .effective_definition_digest()?
                    .as_str()
                    .to_owned(),
                publisher_fingerprint: resolution
                    .root
                    .signer_fingerprint
                    .clone()
                    .context("trusted qualification policy has no publisher")?,
                policy,
            };
            source.validate()?;
            Ok(source)
        },
    )
}

// ----- the two daemons -------------------------------------------------------

/// ORIGIN with a published product.
struct Origin {
    harness: DaemonHarness,
    fixture: FastFixture,
    product: Published,
    _scratch: tempfile::TempDir,
}

async fn start_origin() -> Result<Origin> {
    let (mut harness, fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;
    harness.kill_daemon().await?;
    let scratch = tempfile::tempdir()?;
    let product = publish_product(
        &harness.state_path,
        &scratch.path().join("payload"),
        &operator_principal(),
    )?;
    harness.respawn_with(|_| {}).await?;
    Ok(Origin {
        harness,
        fixture,
        product,
        _scratch: scratch,
    })
}

/// ORIGIN plus the node that receives its product and testimony, and the
/// policy identity HOSTED resolves for the qualification policy.
struct Pair {
    origin: Origin,
    hosted: DaemonHarness,
    hosted_fixture: FastFixture,
    policy_source: ProductQualificationPolicySource,
}

async fn start_pair() -> Result<Pair> {
    let origin_node = common::fast_fixture::node_signing_key();
    let origin_operator = common::fast_fixture::user_signing_key();
    let origin_site = site_of(&origin_node);
    let (mut hosted, hosted_fixture) = DaemonHarness::start_fast_with_node_and_user_keys(
        hosted_node_key(),
        hosted_operator_key(),
        |state_path, _, fixture| {
            authorize_remote_node(
                state_path,
                fixture,
                &origin_node,
                &origin_site,
                ORIGIN_NODE_SCOPES,
            )?;
            authorize_remote_operator(
                state_path,
                fixture,
                &origin_operator,
                &origin_site,
                OPERATOR_SCOPES,
            )?;
            register_config_fixture_bundle(state_path, FIXTURE_BUNDLE, fixture, |bundle_root| {
                plant_policies(bundle_root, &fixture.publisher, &origin_site)
            })
        },
        |_| {},
    )
    .await?;
    let origin = start_origin().await?;
    install_single_remote(
        &origin.harness.state_path,
        remote_config(
            HOSTED_REMOTE,
            format!("http://{}", hosted.bind),
            &hosted_node_key(),
            &hosted_fixture.site_id(),
        ),
    )?;
    install_single_remote(
        &hosted.state_path,
        remote_config(
            ORIGIN_REMOTE,
            format!("http://{}", origin.harness.bind),
            &origin_node,
            &origin_site,
        ),
    )?;
    // The node's engine is built over a stopped node; it keeps its endpoint
    // across the restart.
    hosted.kill_daemon().await?;
    let policy_source = resolve_policy_source(&hosted.state_path, QUALIFICATION_POLICY_REF)?;
    hosted.respawn_with(|_| {}).await?;
    Ok(Pair {
        origin,
        hosted,
        hosted_fixture,
        policy_source,
    })
}

/// A request signer for HOSTED: the operator signs, ORIGIN's node key
/// co-signs as the forwarding authority.
struct HostedClient {
    operator: SigningKey,
    origin_node: SigningKey,
    origin_site_id: String,
    audience: String,
    base: String,
    http: reqwest::Client,
}

impl HostedClient {
    async fn post(&self, path: &str, body: &Value) -> Result<(reqwest::StatusCode, String)> {
        let body_bytes = serde_json::to_vec(body)?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs()
            .to_string();
        let nonce = format!("{:016x}", rand::random::<u64>());
        let body_hash = lillux::cas::sha256_hex(&body_bytes);
        let to_sign = format!(
            "ryeos-request-v1\nPOST\n{path}\n{body_hash}\n{timestamp}\n{nonce}\n{}",
            self.audience
        );
        let content_hash = lillux::cas::sha256_hex(to_sign.as_bytes());
        let signature = lillux::crypto::Signer::sign(&self.operator, content_hash.as_bytes());
        let signature_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let key_id = format!("fp:{}", fingerprint(&self.operator));
        let forwarding_key_id = format!("fp:{}", fingerprint(&self.origin_node));
        let forwarded = format!(
            "ryeos-forwarded-request-v1\nPOST\n{path}\n{body_hash}\n{timestamp}\n{nonce}\n{}\n{key_id}\n{signature_b64}\n{forwarding_key_id}\n{}",
            self.audience, self.origin_site_id
        );
        let forwarded_hash = lillux::cas::sha256_hex(forwarded.as_bytes());
        let forwarding_signature =
            lillux::crypto::Signer::sign(&self.origin_node, forwarded_hash.as_bytes());
        let response = self
            .http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .header("x-ryeos-key-id", key_id)
            .header("x-ryeos-timestamp", timestamp)
            .header("x-ryeos-nonce", nonce)
            .header("x-ryeos-signature", signature_b64)
            .header("x-ryeos-forwarding-key-id", forwarding_key_id)
            .header("x-ryeos-forwarding-site-id", &self.origin_site_id)
            .header(
                "x-ryeos-forwarding-signature",
                base64::engine::general_purpose::STANDARD.encode(forwarding_signature.to_bytes()),
            )
            .body(body_bytes)
            .send()
            .await?;
        let status = response.status();
        Ok((status, response.text().await?))
    }
}

impl Pair {
    fn operator_client(&self) -> HostedClient {
        HostedClient {
            operator: common::fast_fixture::user_signing_key(),
            origin_node: common::fast_fixture::node_signing_key(),
            origin_site_id: self.origin.fixture.site_id(),
            audience: format!("fp:{}", self.hosted_fixture.node_fp()),
            base: format!("http://{}", self.hosted.bind),
            http: reqwest::Client::new(),
        }
    }

    /// Run `service:external-content/push-product` on ORIGIN and return its
    /// result.
    async fn push(&self) -> Result<Value> {
        let product = &self.origin.product;
        let (status, body) = self
            .origin
            .harness
            .post_execute(
                "service:external-content/push-product",
                ".",
                json!({
                    "remote": HOSTED_REMOTE,
                    "witness_hash": product.witness_hash,
                    "origin_admission_hash": product.admission_hash,
                    "maximum_bytes": MAXIMUM_BYTES,
                }),
            )
            .await?;
        ensure!(
            status.is_success(),
            "push-product returned {status}: {body}"
        );
        body.get("result")
            .cloned()
            .with_context(|| format!("push-product response had no result: {body}"))
    }

    /// Submit a signed testimony object to HOSTED's accept route.
    async fn accept(&self, testimony: &Value) -> Result<(reqwest::StatusCode, String)> {
        self.operator_client()
            .post(ACCEPT_PATH, &json!({ "testimony": testimony }))
            .await
    }
}

// ----- the testimony --------------------------------------------------------

/// The origin side of a testimony: the node-signed qualification attestation
/// and the operator key that speaks for the origin.
struct Origination {
    qualification: Attestation,
    origin_site_id: String,
    origin_node_key: lillux::crypto::VerifyingKey,
    operator: KeySigner,
    expires_at: String,
}

impl Origination {
    fn new(pair: &Pair) -> Result<Self> {
        let identity = daemon_identity(&pair.origin.harness.state_path)?;
        let node_signer = NodeIdentitySigner::from_identity(&identity);
        let expires_at = (chrono::Utc::now() + chrono::Duration::days(7))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let evidence = qualification_evidence(&pair.origin.product, pair.policy_source.clone())?;
        let qualification = evidence.sign_attestation(
            &node_signer,
            lillux::time::iso8601_now(),
            Some(expires_at.clone()),
        )?;
        Ok(Self {
            qualification,
            origin_site_id: site_of(&common::fast_fixture::node_signing_key()),
            origin_node_key: *identity.verifying_key(),
            operator: KeySigner::new(common::fast_fixture::user_signing_key()),
            expires_at,
        })
    }

    /// The honest testimony naming `policy_ref`.
    fn testimony(&self, policy_ref: &str) -> Result<Attestation> {
        RemoteQualificationTestimonyBody::new(
            self.qualification.clone(),
            self.origin_site_id.clone(),
            policy_ref.to_owned(),
        )?
        .sign(
            &self.operator,
            &self.origin_node_key,
            lillux::time::iso8601_now(),
            self.expires_at.clone(),
        )
    }

    /// Sign an arbitrary body as a testimony with the operator key, bypassing
    /// the checks the honest signer applies.
    fn forged(&self, body: Value) -> Result<Attestation> {
        Attestation::unsigned(
            self.qualification.subject_hash.clone(),
            REMOTE_QUALIFICATION_TESTIMONY_CLAIM.to_owned(),
            REMOTE_QUALIFICATION_TESTIMONY_POLICY.to_owned(),
            lillux::time::iso8601_now(),
            Some(self.expires_at.clone()),
            body,
        )
        .sign(&self.operator)
    }

    fn honest_body(&self, policy_ref: &str) -> Result<Value> {
        Ok(serde_json::to_value(
            RemoteQualificationTestimonyBody::new(
                self.qualification.clone(),
                self.origin_site_id.clone(),
                policy_ref.to_owned(),
            )?,
        )?)
    }
}

fn assert_refused(status: reqwest::StatusCode, text: &str, needle: &str, what: &str) {
    assert!(
        !status.is_success() && text.contains(needle),
        "{what}: expected a refusal containing {needle:?}, got {status}: {text}"
    );
}

fn assert_hash(value: &Value, what: &str) -> String {
    let hash = value.as_str().unwrap_or_default();
    assert!(
        lillux::valid_hash(hash) && !hash.bytes().any(|byte| byte.is_ascii_uppercase()),
        "{what} is not a canonical hash: {value}"
    );
    hash.to_owned()
}

/// A testimony is accepted only for a pushed product, only under a receiver
/// policy that names the attester and the verifier, and only when every
/// embedded signature and derivation holds. The acceptance is idempotent and
/// is never published as a local qualification.
#[tokio::test(flavor = "multi_thread")]
async fn testimony_is_accepted_only_under_receiver_policy() -> Result<()> {
    let mut pair = start_pair().await?;
    let origination = Origination::new(&pair)?;
    let honest = origination.testimony(ACCEPT_POLICY_REF)?;
    let honest_value = honest.to_value();
    let testimony_hash = canonical_value_digest(&honest_value)?;

    // The product has not crossed yet: nothing to attach the testimony to.
    let (status, text) = pair.accept(&honest_value).await?;
    assert_refused(
        status,
        &text,
        "was not received from this origin by this operator",
        "testimony for a product that was never pushed",
    );

    let pushed = pair.push().await?;
    assert_hash(&pushed["acceptance_hash"], "product acceptance_hash");

    // A receiver policy that does not name this attester gains nothing from
    // the testimony.
    let unlisted = origination.testimony(UNLISTED_POLICY_REF)?;
    let (status, text) = pair.accept(&unlisted.to_value()).await?;
    assert_refused(
        status,
        &text,
        "attester is not trusted for this origin site",
        "policy without the attester",
    );

    // A policy that pins other verifier digests refuses this verifier.
    let stale = origination.testimony(STALE_POLICY_REF)?;
    let (status, text) = pair.accept(&stale.to_value()).await?;
    assert_refused(
        status,
        &text,
        "stale verifier identity",
        "verifier digest outside the accepted list",
    );

    // An embedded attestation hash that disagrees with the embedded bytes is
    // refused even though the operator signed it.
    let mut body = origination.honest_body(ACCEPT_POLICY_REF)?;
    body["qualification_attestation_hash"] = json!("e".repeat(64));
    let (status, text) = pair.accept(&origination.forged(body)?.to_value()).await?;
    assert_refused(
        status,
        &text,
        "differs from the embedded attestation",
        "tampered embedded attestation hash",
    );

    // An embedded attestation signed by a key that is not the pinned origin
    // node key is refused even though the operator signed the testimony.
    let rogue = KeySigner::new(rogue_node_key());
    let rogue_evidence = qualification_evidence(&pair.origin.product, pair.policy_source.clone())?;
    let rogue_attestation = rogue_evidence.sign_attestation(
        &rogue,
        lillux::time::iso8601_now(),
        Some(origination.expires_at.clone()),
    )?;
    let rogue_body = RemoteQualificationTestimonyBody::new(
        rogue_attestation,
        origination.origin_site_id.clone(),
        ACCEPT_POLICY_REF.to_owned(),
    )?;
    let (status, text) = pair
        .accept(
            &origination
                .forged(serde_json::to_value(&rogue_body)?)?
                .to_value(),
        )
        .await?;
    assert!(
        !status.is_success(),
        "an attestation signed by an unpinned node key was accepted: {text}"
    );

    // A body changed after the operator signed it fails the testimony
    // signature.
    let mut mutated = honest_value.clone();
    mutated["evidence"]["asserted_claims"] = json!(["extension_probe"]);
    let (status, text) = pair.accept(&mutated).await?;
    assert!(
        !status.is_success(),
        "a testimony changed after signing was accepted: {text}"
    );

    // The honest testimony is accepted under the policy that names it.
    let (status, text) = pair.accept(&honest_value).await?;
    ensure!(status.is_success(), "accept returned {status}: {text}");
    let accepted: Value = serde_json::from_str(&text)?;
    assert_eq!(accepted["testimony_hash"], testimony_hash.as_str());
    let acceptance_hash = assert_hash(&accepted["acceptance_hash"], "acceptance_hash");
    let coordinate_id = assert_hash(&accepted["coordinate_id"], "coordinate_id");
    assert_eq!(accepted["reused_existing"], false);
    assert_ne!(
        acceptance_hash, testimony_hash,
        "the acceptance is the receiver's own record, not the carried testimony"
    );

    // Accepting the same testimony again returns the recorded acceptance.
    let (status, text) = pair.accept(&honest_value).await?;
    ensure!(
        status.is_success(),
        "second accept returned {status}: {text}"
    );
    let again: Value = serde_json::from_str(&text)?;
    assert_eq!(again["acceptance_hash"], acceptance_hash.as_str());
    assert_eq!(again["coordinate_id"], coordinate_id.as_str());
    assert_eq!(again["reused_existing"], true);

    // The origin refuses to export a qualification it does not hold, and
    // sends nothing to HOSTED.
    let (status, body) = pair
        .origin
        .harness
        .post_execute(
            "service:external-content/export-qualification-testimony",
            ".",
            json!({
                "remote": HOSTED_REMOTE,
                "qualification_hash": "a".repeat(64),
                "acceptance_policy_ref": ACCEPT_POLICY_REF,
            }),
        )
        .await?;
    assert!(
        !status.is_success(),
        "export of an unheld qualification succeeded: {body}"
    );

    // The acceptance is HOSTED's published head in the testimony namespace,
    // and no local qualification head exists for the coordinate.
    pair.hosted.kill_daemon().await?;
    let store = open_daemon_state(&pair.hosted.state_path)?;
    let head = store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                REMOTE_QUALIFICATION_TESTIMONY_HEAD_NAMESPACE,
                &coordinate_id,
            )
        })?
        .context("HOSTED has no acceptance head for the coordinate")?;
    assert_eq!(head.target_hash, acceptance_hash);
    let local = store.with_state_db(|db| {
        db.read_generic_head_ref(PRODUCT_QUALIFICATION_HEAD_NAMESPACE, &coordinate_id)
    })?;
    assert!(
        local.is_none(),
        "a remote testimony was published as a local qualification"
    );
    Ok(())
}
