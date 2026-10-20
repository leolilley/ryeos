//! Real runtime qualification for the synthetic external-candidate adapter.
//!
//! Two independent attestations are produced here, each only through the
//! production owners:
//!
//! * a guest-runtime product qualification, carrying the guest mechanism
//!   claims a signed placement binding selects as `runtime_qualification`;
//! * an activated-content qualification, carrying the session runtime claims a
//!   signed placement binding selects as `runtime_content_qualification`, after
//!   the runtime bytes were acquired by a real managed activation.
//!
//! The authoring half writes signed items into an existing signed authoring
//! bundle before its node starts; the flow half runs against that live node.
//! Neither half writes testimony: claims come from the verifier's retained
//! execution and are published by the daemon's qualification operations.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::candidate_authoring::{qualification_policy_source, qualification_verifier_tool_source};
use super::qualification_node::{
    ManagedActivationGrant, RetainedTestRuntime, bind_manifests_to_tool,
    bind_typed_manifests_to_tool, operator_context,
};

/// Mechanism claims a guest-runtime verifier may attest. A signed placement
/// binding selects the subset its workload needs.
pub const GUEST_RUNTIME_CLAIMS: [&str; 4] = [
    ryeos_external_execution_contract::CAPTURED_INPUTS_SUPPORT_CLAIM,
    ryeos_external_execution_contract::DIRECT_PROGRAM_SUPPORT_CLAIM,
    ryeos_external_execution_contract::TARGET_LIMITS_SUPPORT_CLAIM,
    ryeos_external_execution_contract::GUEST_WIRE_SUPPORT_CLAIM,
];

/// The claims a binding for an ordinary or direct guest requires by default.
pub const GUEST_RUNTIME_REQUIRED_CLAIMS: [&str; 3] = [
    ryeos_external_execution_contract::CAPTURED_INPUTS_SUPPORT_CLAIM,
    ryeos_external_execution_contract::DIRECT_PROGRAM_SUPPORT_CLAIM,
    ryeos_external_execution_contract::GUEST_WIRE_SUPPORT_CLAIM,
];

pub const GUEST_RUNTIME_RECIPE_REF: &str = "config:fixtures/guest_runtime_recipe";
pub const GUEST_RUNTIME_POLICY_REF: &str = "config:fixtures/guest-runtime-qualification";
pub const GUEST_RUNTIME_VERIFIER_REF: &str = "tool:fixtures/verify-guest-runtime";
pub const GUEST_RUNTIME_RELATIONSHIP: &str = "guest_runtime_to_verifier";
pub const GUEST_RUNTIME_PRODUCT: &str = "guest_runtime";

pub const CONTENT_ACTIVATION_REF: &str = "config:fixtures/runtime-activation";
pub const CONTENT_POLICY_REF: &str = "config:fixtures/runtime-content-qualification";
pub const CONTENT_VERIFIER_REF: &str = "tool:fixtures/verify-activated-runtime";
pub const CONTENT_CONSUMER_REF: &str = "tool:fixtures/runtime-consumer";
pub const CONTENT_DECLARATION_ID: &str = "runtime";
pub const CONTENT_ARCHIVE_ROOT: &str = "synthetic-archives";

const ARCHIVE_FILE_NAME: &str = "synthetic-runtime.tar.gz";

fn sign(identity: &ryeos_node_runtime::identity::NodeIdentity, source: &str) -> String {
    lillux::signature::sign_content(source, identity.signing_key(), "#", None)
}

fn write_signed(
    bundle_root: &Path,
    relative: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    source: &str,
) {
    let path = bundle_root.join(".ai").join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, sign(identity, source)).unwrap();
}

// ---------------------------------------------------------------------------
// Guest-runtime product qualification
// ---------------------------------------------------------------------------

/// Verifier parameters of the guest-runtime policy. The guest verifier has no
/// Worker consumer context; the object only keeps the policy and its tool
/// bound to one another.
pub fn guest_runtime_verifier_parameters() -> serde_json::Value {
    serde_json::json!({"guest_runtime_qualification": "synthetic_guest_runtime_v1"})
}

/// How a guest-runtime product is stored and bounded by its signed relationship.
pub struct GuestProductStorage {
    pub storage: &'static str,
    pub maximum_file_bytes: u64,
    pub maximum_total_bytes: u64,
}

fn guest_runtime_recipe_source(
    guest_runtime_manifest_hash: &str,
    required_claims: &[&str],
    product: &GuestProductStorage,
) -> String {
    use ryeos_state::external_content::products::composition::ProductRelationships;
    use ryeos_state::external_content::products::{
        PRODUCT_DECLARATIONS_SCHEMA, ProductDeclaration, ProductDeclarations, ProductSource,
    };

    let claims = required_claims
        .iter()
        .map(|claim| format!("          - {claim}\n"))
        .collect::<String>();
    let relationship = format!(
        r#"category: fixtures
version: "1.0.0"
description: Exact synthetic guest-runtime relationship
product_relationships:
  schema: ryeos.product_relationships.v1
  relationships:
    - name: {GUEST_RUNTIME_RELATIONSHIP}
      producer:
        canonical_ref: graph:fixtures/build_guest_runtime
        recipe_binding: product_recipe
        product_name: {GUEST_RUNTIME_PRODUCT}
        parameters: {{}}
      consumer:
        canonical_ref: worker:test/external-candidate
        declaration_id: auxiliary
      required_product:
        shape: tree
        storage: {storage}
        bounds:
          maximum_entries: 16
          maximum_depth: 4
          maximum_file_bytes: {maximum_file_bytes}
          maximum_total_bytes: {maximum_total_bytes}
      qualification:
        policy_ref: {GUEST_RUNTIME_POLICY_REF}
        required_claims:
{claims}"#,
        storage = product.storage,
        maximum_file_bytes = product.maximum_file_bytes,
        maximum_total_bytes = product.maximum_total_bytes,
    );
    let parsed: serde_json::Value = serde_yaml::from_str(&relationship).unwrap();
    let relationships =
        ProductRelationships::from_value(parsed["product_relationships"].clone()).unwrap();
    let allowance = relationships.select(GUEST_RUNTIME_RELATIONSHIP).unwrap();
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: Vec::new(),
        products: vec![ProductDeclaration {
            name: allowance.producer.product_name.clone(),
            source: ProductSource::RetainedProject {},
            path: "products/guest-runtime".into(),
            shape: allowance.required_product.shape,
            storage: allowance.required_product.storage,
            required: true,
            bounds: allowance.required_product.bounds.clone(),
            expected_manifest_hash: Some(guest_runtime_manifest_hash.into()),
        }],
    };
    let source = format!(
        "{relationship}\n{}",
        serde_yaml::to_string(&serde_json::json!({"build_products": declarations})).unwrap()
    );
    let parsed: serde_json::Value = serde_yaml::from_str(&source).unwrap();
    let declarations = ProductDeclarations::from_value(parsed["build_products"].clone()).unwrap();
    let relationships =
        ProductRelationships::from_value(parsed["product_relationships"].clone()).unwrap();
    relationships
        .validate_against(&declarations, "product_recipe")
        .unwrap();
    source
}

/// Write the guest-runtime recipe, policy and verifier into a signed authoring
/// bundle, before its node starts, for a guest runtime stored as `product`.
/// `required_claims` is the relationship's selection and must be a sorted
/// subset of [`GUEST_RUNTIME_CLAIMS`]; the policy allows, and the verifier probe
/// attests, all of them. The verifier is a distinct tool from any other
/// policy's, because its `config_schema` pins this policy's parameters.
pub fn write_guest_runtime_sources_for(
    bundle_root: &Path,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    guest_runtime_manifest_hash: &str,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
    product: &GuestProductStorage,
) {
    assert!(
        required_claims.windows(2).all(|pair| pair[0] < pair[1])
            && required_claims
                .iter()
                .all(|claim| GUEST_RUNTIME_CLAIMS.contains(claim)),
        "guest runtime required claims must be a sorted subset of the attested claims"
    );
    let parameters = guest_runtime_verifier_parameters();
    write_signed(
        bundle_root,
        "config/fixtures/guest_runtime_recipe.yaml",
        identity,
        &guest_runtime_recipe_source(guest_runtime_manifest_hash, required_claims, product),
    );
    write_signed(
        bundle_root,
        "config/fixtures/guest-runtime-qualification.yaml",
        identity,
        &qualification_policy_source(
            "Exact synthetic guest runtime qualification",
            GUEST_RUNTIME_VERIFIER_REF,
            &GUEST_RUNTIME_CLAIMS,
            &parameters,
        ),
    );
    write_signed(
        bundle_root,
        "tools/fixtures/verify-guest-runtime.yaml",
        identity,
        &qualification_verifier_tool_source(
            "verify-guest-runtime",
            "Execute the exact synthetic guest runtime's independent qualification probe",
            guest_runtime_manifest_hash,
            verifier_runtime_manifest_hash,
            &GUEST_RUNTIME_CLAIMS,
            &parameters,
        ),
    );
}

/// What a signed placement binding needs to select a guest-runtime proof:
/// `ExternalRuntimeQualificationBinding { attestation_hash, owner_principal,
/// qualification: ProductRelationshipQualification { policy_ref: Some(policy_ref),
/// required_claims } }`.
#[derive(Debug, Clone)]
pub struct GuestRuntimeQualification {
    pub attestation_hash: String,
    pub owner_principal: String,
    pub policy_ref: String,
    pub required_claims: Vec<String>,
    pub guest_runtime_manifest_hash: String,
}

/// The identity a guest-runtime product witness names, independent of whether
/// the tree is stored as a large or an ordinary content manifest.
pub struct GuestProductSubject {
    pub manifest_hash: String,
    pub manifest_kind: &'static str,
    pub entry_count: usize,
    pub total_bytes: u64,
}

/// Publish the product witness for one guest runtime tree already resident in
/// the node, against the recipe [`write_guest_runtime_sources_for`] authored.
pub fn publish_guest_runtime_witness(
    state: &ryeos_app::state::AppState,
    subject: &GuestProductSubject,
) -> String {
    use ryeos_engine::contracts::SubjectResolutionAuthority;
    use ryeos_state::external_content::products::publication::{
        ProductCaptureCoordinate, publish_product_witness,
    };
    use ryeos_state::external_content::products::{
        PRODUCT_CAPTURE_EVIDENCE_SCHEMA, PRODUCT_DECLARATIONS_SCHEMA, ProductCaptureEvidence,
        ProductDeclaration, ProductDeclarations, ProductProducerAdmission, ProductSource,
    };

    let relationship_resolution = state
        .engine
        .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(GUEST_RUNTIME_RECIPE_REF)
                .unwrap(),
            expected_kind: Some("config".into()),
            project_root: None,
            subject_resolution_authority: SubjectResolutionAuthority::Projectless,
        })
        .unwrap();
    let relationships =
        ryeos_state::external_content::products::composition::ProductRelationships::from_value(
            relationship_resolution.composed.composed["product_relationships"].clone(),
        )
        .unwrap();
    let relationship = relationships
        .select(GUEST_RUNTIME_RELATIONSHIP)
        .unwrap()
        .clone();
    let declaration = ProductDeclaration {
        name: relationship.producer.product_name.clone(),
        source: ProductSource::RetainedProject {},
        path: "products/guest-runtime".into(),
        shape: relationship.required_product.shape,
        storage: relationship.required_product.storage,
        required: true,
        bounds: relationship.required_product.bounds.clone(),
        expected_manifest_hash: Some(subject.manifest_hash.clone()),
    };
    let declarations = ProductDeclarations {
        schema: PRODUCT_DECLARATIONS_SCHEMA.into(),
        output_roots: Vec::new(),
        products: vec![declaration.clone()],
    };
    let producer = ProductProducerAdmission {
        canonical_ref: relationship.producer.canonical_ref.clone(),
        effective_definition_digest: "a".repeat(64),
        exact_program_hash: "b".repeat(64),
        producer_project_snapshot_hash: "c".repeat(64),
        launch_authority_digest: "d".repeat(64),
        admitted_parameters_digest: relationship.producer.admitted_parameters_digest().unwrap(),
    };
    let operator =
        ryeos_node_runtime::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
            .unwrap();
    let evidence = ProductCaptureEvidence {
        schema: PRODUCT_CAPTURE_EVIDENCE_SCHEMA,
        owner_principal: operator.principal_id(),
        chain_root_id: "T-20000000-0000-0000-0000-000000000001".into(),
        thread_id: "T-20000000-0000-0000-0000-000000000002".into(),
        admitted_launch_capsule_hash: "e".repeat(64),
        producer: producer.clone(),
        root_producer: producer,
        result_project_snapshot_hash: "f".repeat(64),
        workspace_output_capture_hash: None,
        producer_partition_identity: None,
        recipe_purpose:
            ryeos_state::external_content::products::ProductRecipePurpose::GeneralProductV1,
        recipe_binding: relationship.producer.recipe_binding.clone(),
        recipe_ref: relationship_resolution.root.resolved_ref.clone(),
        recipe_raw_content_digest: relationship_resolution.root.raw_content_digest.clone(),
        declarations_hash: declarations.content_hash().unwrap(),
        declarations,
        relationships,
        declaration,
        capture_policy_digest: "9".repeat(64),
        manifest_hash: subject.manifest_hash.clone(),
        manifest_kind: subject.manifest_kind.into(),
        entry_count: subject.entry_count,
        total_bytes: subject.total_bytes,
    };
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_exclusive_guard(true).unwrap();
    let signer = ryeos_app::state_store::NodeIdentitySigner::from_identity(&state.identity);
    let attestation = evidence
        .sign_attestation(&signer, "2026-09-23T00:00:00Z".into())
        .unwrap();
    publish_product_witness(
        &authority,
        &ProductCaptureCoordinate::from_evidence(&evidence).unwrap(),
        &attestation,
        state
            .node_policy
            .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()
            .unwrap()
            .closure_limits()
            .unwrap(),
        &signer,
        &guard,
    )
    .unwrap()
    .witness
    .attestation_hash
}

/// Run `body` on a dedicated multi-thread runtime that stays alive, so an
/// accepted launch's background dispatch is not cancelled when the launch
/// request returns. The runtime is returned for the caller to retain.
fn launch_on_retained_runtime<T: Send + 'static>(
    name: &str,
    body: impl FnOnce(&tokio::runtime::Runtime) -> T + Send + 'static,
) -> (T, RetainedTestRuntime) {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .unwrap();
            let value = body(&runtime);
            (value, RetainedTestRuntime(Some(runtime)))
        })
        .unwrap()
        .join()
        .unwrap()
}

async fn wait_for_completed_thread(state: &ryeos_app::state::AppState, thread_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let thread = state
            .state_store
            .get_thread(thread_id)
            .unwrap()
            .expect("accepted qualification thread disappeared");
        if ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
            .is_some_and(|status| status.is_terminal())
        {
            assert_eq!(
                thread.status, "completed",
                "qualification verifier failed; root={thread_id}; detail={thread:#?}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "qualification verifier did not settle; root={thread_id}; detail={thread:#?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Launch tasks release their state handles shortly after a verifier settles.
/// Wait for that so the caller can recover sole ownership of the application.
async fn wait_for_sole_state_owner(state: &Arc<ryeos_app::state::AppState>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Arc::strong_count(state) > 1 {
        assert!(
            Instant::now() < deadline,
            "qualification tasks retained the application after the verifier settled"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Qualify one guest runtime product, already resident in the node and
/// witnessed by `witness_hash` (see [`publish_guest_runtime_witness`]), against
/// the sources a guest-runtime source writer authored. Requires the node to be
/// started with that bundle. On return the caller is the sole owner of `state`
/// again.
pub async fn qualify_published_guest_runtime(
    state: &Arc<ryeos_app::state::AppState>,
    subject: &GuestProductSubject,
    witness_hash: String,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
) -> GuestRuntimeQualification {
    let guest_hash = subject.manifest_hash.clone();
    let context = operator_context(state);
    bind_typed_manifests_to_tool(
        state,
        GUEST_RUNTIME_VERIFIER_REF,
        &[
            (guest_hash.clone(), subject.manifest_kind.to_owned()),
            (
                verifier_runtime_manifest_hash.to_owned(),
                ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
            ),
        ],
    );
    let launch_state = state.clone();
    let launch_context = context.clone();
    let launch_witness = witness_hash.clone();
    let (launch, launch_runtime) = launch_on_retained_runtime(
        "guest-runtime-qualification-launch",
        move |runtime| {
            runtime
                .block_on(ryeos_api::handlers::product_qualification_launch::handle(
                    ryeos_api::handlers::product_qualification_launch::Request {
                        consumer_invocation: None,
                        launch_id: format!("L-{}", "b".repeat(32)),
                        witness_hash: launch_witness,
                        witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                        relationship_name: GUEST_RUNTIME_RELATIONSHIP.into(),
                        project_context: None,
                        auxiliary_product_selections: Vec::new(),
                    },
                    launch_context,
                    launch_state,
                ))
                .unwrap()
        },
    );
    let verifier_thread_id = launch["thread_id"]
        .as_str()
        .expect("qualification launch returned no exact verifier thread")
        .to_owned();
    wait_for_completed_thread(state, &verifier_thread_id).await;
    let qualification = ryeos_app::operator_external_content::product_qualification::qualify(
        state.clone(),
        context.clone(),
        ryeos_app::operator_external_content::product_qualification::ProductQualificationRequest {
            witness_hash: witness_hash.clone(),
            witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
            relationship_name: GUEST_RUNTIME_RELATIONSHIP.into(),
            verifier_chain_root_id: verifier_thread_id.clone(),
            verifier_thread_id,
        },
    )
    .await
    .unwrap();
    drop(launch_runtime);
    wait_for_sole_state_owner(state).await;
    // The retained qualification must re-admit against the current signed
    // policy, verifier and artifact identity, with its testimony still the
    // published head.
    let limits = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()
        .unwrap()
        .closure_limits()
        .unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let measured_state = state.clone();
    let measured_authority = authority.try_clone().unwrap();
    let measured_context = context.clone();
    let measured_hash = qualification.qualification_hash.clone();
    let owner = context.fingerprint.clone();
    let measured = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_external_content::product_qualification::measure_current_qualification_authority(
            &measured_state,
            &measured_context,
            &measured_authority,
            limits,
            &owner,
            &measured_hash,
        )
    })
    .await
    .unwrap()
    .unwrap();
    for claim in required_claims {
        assert!(
            measured
                .required_qualification_claims
                .iter()
                .any(|attested| attested == claim),
            "guest runtime qualification did not attest {claim}"
        );
    }
    assert_eq!(
        measured.qualification_policy.canonical_ref,
        GUEST_RUNTIME_POLICY_REF
    );
    wait_for_sole_state_owner(state).await;
    GuestRuntimeQualification {
        attestation_hash: qualification.qualification_hash,
        owner_principal: context.fingerprint.clone(),
        policy_ref: GUEST_RUNTIME_POLICY_REF.to_owned(),
        required_claims: required_claims
            .iter()
            .map(|claim| (*claim).to_owned())
            .collect(),
        guest_runtime_manifest_hash: guest_hash,
    }
}

// ---------------------------------------------------------------------------
// Activated-content qualification
// ---------------------------------------------------------------------------

/// A runtime tree packed as the managed-activation archive the offline
/// acquisition path consumes.
pub struct RuntimeArchive {
    pub file_name: String,
    pub sha256: String,
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
    pub largest_file_bytes: u64,
    pub total_file_bytes: u64,
    bytes: Vec<u8>,
}

impl RuntimeArchive {
    /// Pack `runtime` under a single `runtime/` prefix directory. Compression
    /// is stored-only; the archive is byte-for-byte deterministic.
    pub fn from_tree(runtime: &Path) -> Self {
        let mut tar = Vec::new();
        let mut largest = 0u64;
        let mut total = 0u64;
        tar_directory_header(&mut tar, "runtime/");
        walk_tree(runtime, "runtime", &mut |relative, path, is_dir| {
            if is_dir {
                tar_directory_header(&mut tar, &format!("{relative}/"));
            } else {
                let bytes = std::fs::read(path).unwrap();
                largest = largest.max(bytes.len() as u64);
                total += bytes.len() as u64;
                tar_file(&mut tar, relative, &bytes);
            }
        });
        tar.extend_from_slice(&[0u8; 1024]);
        let expanded_bytes = tar.len() as u64;
        let bytes = gzip_stored(&tar);
        Self {
            file_name: ARCHIVE_FILE_NAME.to_owned(),
            sha256: lillux::sha256_hex(&bytes),
            compressed_bytes: bytes.len() as u64,
            expanded_bytes,
            largest_file_bytes: largest,
            total_file_bytes: total,
            bytes,
        }
    }

    /// Place the archive in the offline root the node policy admits.
    pub fn write_into(&self, archive_root: &Path) {
        std::fs::write(archive_root.join(&self.file_name), &self.bytes).unwrap();
    }

    /// Node policy sized exactly to this archive, with generous slack only for
    /// header overhead.
    pub fn activation_grant(&self, archive_root: &Path) -> ManagedActivationGrant {
        let mib = 1024 * 1024;
        let round = |value: u64| value.div_ceil(mib) * mib + 16 * mib;
        ManagedActivationGrant {
            archive_root_name: CONTENT_ARCHIVE_ROOT.to_owned(),
            archive_root: archive_root.to_owned(),
            limits: ryeos_node_runtime::node_policy::sections::external_content::ManagedExternalContentActivationPolicy {
                allow_online: false,
                allowed_https_hosts: vec!["archives.example.test".to_owned()],
                max_redirects: 0,
                max_archives: 1,
                max_compressed_bytes: round(self.compressed_bytes),
                max_expanded_bytes: round(self.expanded_bytes),
                max_members: 64,
                max_member_bytes: round(self.largest_file_bytes),
                max_concurrent_activations: 1,
                cache_budget_bytes: 2 * round(self.compressed_bytes),
                store_budget_bytes: 2 * round(self.expanded_bytes),
                minimum_free_bytes: 64 * mib,
                max_attempts: 2,
            },
        }
    }
}

pub fn walk_tree(root: &Path, prefix: &str, visit: &mut dyn FnMut(&str, &Path, bool)) {
    let mut names = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    for name in names {
        let path = root.join(&name);
        let relative = format!("{prefix}/{name}");
        let is_dir = std::fs::symlink_metadata(&path).unwrap().is_dir();
        visit(&relative, &path, is_dir);
        if is_dir {
            walk_tree(&path, &relative, visit);
        }
    }
}

fn tar_header(name: &str, mode: u32, size: u64, type_flag: u8) -> [u8; 512] {
    assert!(
        name.len() <= 100,
        "synthetic archive path exceeds ustar name field"
    );
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    let octal = |value: u64, width: usize| format!("{value:0width$o}\0", width = width - 1);
    header[100..108].copy_from_slice(octal(u64::from(mode), 8).as_bytes());
    header[108..116].copy_from_slice(octal(0, 8).as_bytes());
    header[116..124].copy_from_slice(octal(0, 8).as_bytes());
    header[124..136].copy_from_slice(octal(size, 12).as_bytes());
    header[136..148].copy_from_slice(octal(0, 12).as_bytes());
    header[148..156].copy_from_slice(b"        ");
    header[156] = type_flag;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    header
}

fn tar_directory_header(tar: &mut Vec<u8>, name: &str) {
    tar.extend_from_slice(&tar_header(name, 0o755, 0, b'5'));
}

fn tar_file(tar: &mut Vec<u8>, name: &str, bytes: &[u8]) {
    tar.extend_from_slice(&tar_header(name, 0o755, bytes.len() as u64, b'0'));
    tar.extend_from_slice(bytes);
    tar.resize(tar.len().div_ceil(512) * 512, 0);
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut value = index as u32;
        for _ in 0..8 {
            value = if value & 1 == 1 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
        }
        *slot = value;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes {
        crc = table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    let chunks = data.chunks(65_535).collect::<Vec<_>>();
    for (index, chunk) in chunks.iter().enumerate() {
        out.push(u8::from(index + 1 == chunks.len()));
        out.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Everything the signed activated-content items are derived from.
pub struct ActivatedRuntimeSources<'a> {
    /// Manifest identity of the tree the archive acquires (see
    /// `predict_large_manifest`); the signed consumer pins it.
    pub runtime_manifest_hash: &'a str,
    /// Manifest identity of the independent verifier runtime, resident in the
    /// node before the verifier executes.
    pub verifier_runtime_manifest_hash: &'a str,
    pub archive: &'a RuntimeArchive,
    /// The policy's `verifier_parameters`. A caller that wants its program to
    /// admit this proof passes the admission's qualification-use parameters.
    pub verifier_parameters: &'a serde_json::Value,
    /// Claims the consumer's allowance requires, sorted; must be allowed by the
    /// policy below, which allows exactly [`CONTENT_RUNTIME_CLAIMS`].
    pub required_claims: &'a [&'a str],
}

/// The session runtime claims the content verifier attests.
pub const CONTENT_RUNTIME_CLAIMS: &[&str] =
    ryeos_state::external_execution::admission::REQUIRED_CLAIMS;

/// Write the activation, consumer, policy and verifier for one acquired
/// runtime into a signed authoring bundle, before its node starts.
pub fn write_activated_runtime_sources(
    bundle_root: &Path,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    sources: &ActivatedRuntimeSources<'_>,
) {
    assert!(
        sources
            .required_claims
            .windows(2)
            .all(|pair| pair[0] < pair[1])
            && sources
                .required_claims
                .iter()
                .all(|claim| CONTENT_RUNTIME_CLAIMS.contains(claim)),
        "content required claims must be a sorted subset of the attested claims"
    );
    let archive = sources.archive;
    let mib = 1024 * 1024;
    let activation = format!(
        r#"category: fixtures
schema: ryeos.external_content_activation.v3
consumer_ref: {CONTENT_CONSUMER_REF}
sources:
  - id: runtime-archive
    url: https://archives.example.test/{file_name}
    archive_format: tar_gzip
    sha256: {sha256}
    maximum_compressed_bytes: {compressed}
    maximum_expanded_bytes: {expanded}
    maximum_entries: 17
components:
  - id: {CONTENT_DECLARATION_ID}
    storage: large_content
    shape:
      kind: whole_archive_tree
      source: runtime-archive
      prefix: runtime
      bounds:
        maximum_entries: 16
        maximum_depth: 4
        maximum_file_bytes: {file_bound}
        maximum_total_bytes: {total_bound}
"#,
        file_name = archive.file_name,
        sha256 = archive.sha256,
        compressed = archive.compressed_bytes.div_ceil(mib) * mib + 16 * mib,
        expanded = archive.expanded_bytes.div_ceil(mib) * mib + 16 * mib,
        file_bound = archive.largest_file_bytes.div_ceil(mib) * mib + 16 * mib,
        total_bound = archive.total_file_bytes.div_ceil(mib) * mib + 16 * mib,
    );
    write_signed(
        bundle_root,
        "config/fixtures/runtime-activation.yaml",
        identity,
        &activation,
    );
    let claims = sources
        .required_claims
        .iter()
        .map(|claim| format!("        - {claim}\n"))
        .collect::<String>();
    let consumer = format!(
        r#"category: fixtures
name: runtime-consumer
version: "1.0.0"
description: Installed consumer of the activated synthetic session runtime
executor_id: "@subprocess"
execution_protocol: protocol:ryeos/core/opaque
effects: live
filesystem_authority: node_policy
network_authority: isolated
external_content:
  - id: {CONTENT_DECLARATION_ID}
    kind: tree
    mode: pinned
    digest: {runtime_manifest_hash}
    metadata_hint: activated-synthetic-external-candidate-runtime
    mount_root: execution_runtime
    mount: runtime
    qualification_allowance:
      activation_ref: {CONTENT_ACTIVATION_REF}
      policy_ref: {CONTENT_POLICY_REF}
      required_claims:
{claims}config:
  command: /bin/true
  args: []
  env: {{}}
  timeout_secs: 30
config_schema:
  type: object
  additionalProperties: false
"#,
        runtime_manifest_hash = sources.runtime_manifest_hash,
    );
    write_signed(
        bundle_root,
        "tools/fixtures/runtime-consumer.yaml",
        identity,
        &consumer,
    );
    write_signed(
        bundle_root,
        "config/fixtures/runtime-content-qualification.yaml",
        identity,
        &qualification_policy_source(
            "Exact synthetic activated session runtime qualification",
            CONTENT_VERIFIER_REF,
            CONTENT_RUNTIME_CLAIMS,
            sources.verifier_parameters,
        ),
    );
    write_signed(
        bundle_root,
        "tools/fixtures/verify-activated-runtime.yaml",
        identity,
        &qualification_verifier_tool_source(
            "verify-activated-runtime",
            "Execute the exact acquired synthetic runtime's independent qualification probe",
            sources.runtime_manifest_hash,
            sources.verifier_runtime_manifest_hash,
            CONTENT_RUNTIME_CLAIMS,
            sources.verifier_parameters,
        ),
    );
}

/// What a signed placement binding needs to select an activated-content proof:
/// `ExternalRuntimeContentQualificationBinding { activation_ref, coordinate_id,
/// attestation_hash, owner_principal, required_claims }`.
#[derive(Debug, Clone)]
pub struct ActivatedRuntimeQualification {
    pub activation_ref: String,
    pub coordinate_id: String,
    pub attestation_hash: String,
    pub owner_principal: String,
    pub required_claims: Vec<String>,
}

/// Acquire the archived runtime by a real managed activation (offline, through
/// the node-admitted archive root) and return the activation receipt hash. The
/// node must have been started with `RuntimeArchive::activation_grant` and the
/// sources written by [`write_activated_runtime_sources`]; the archive must
/// already be in the root.
pub async fn activate_runtime(state: &Arc<ryeos_app::state::AppState>) -> String {
    let context = operator_context(state);
    let activation = ryeos_api::handlers::external_content_activate::handle(
        ryeos_api::handlers::external_content_activate::Request {
            activation_ref: CONTENT_ACTIVATION_REF.to_owned(),
            mode: ryeos_app::managed_external_content_operation::AcquisitionMode::Offline,
            offline_archive_root: Some(CONTENT_ARCHIVE_ROOT.to_owned()),
        },
        context.clone(),
        state.clone(),
    )
    .await
    .unwrap();
    let activation: ryeos_api::handlers::external_content_activate::Response =
        serde_json::from_value(activation).unwrap();
    wait_for_activation(state, &activation.job_id).await
}

/// Run the content verifier as an accepted qualification launch against the
/// runtime [`activate_runtime`] acquired and publish its testimony. On return
/// the caller is the sole owner of `state` again.
pub async fn qualify_activated_runtime(
    state: &Arc<ryeos_app::state::AppState>,
    runtime_manifest_hash: &str,
    verifier_runtime_manifest_hash: &str,
    required_claims: &[&str],
) -> ActivatedRuntimeQualification {
    let context = operator_context(state);
    bind_manifests_to_tool(
        state,
        CONTENT_VERIFIER_REF,
        &[runtime_manifest_hash, verifier_runtime_manifest_hash],
    );
    let launch_id = format!("L-{}", "c".repeat(32));
    let launch_state = state.clone();
    let launch_context = context.clone();
    let launch_id_for_launch = launch_id.clone();
    let (launch, launch_runtime) =
        launch_on_retained_runtime("content-qualification-launch", move |runtime| {
            runtime
                .block_on(
                    (ryeos_api::handlers::product_qualification_launch::CONTENT_DESCRIPTOR.handler)(
                        serde_json::json!({
                            "launch_id": launch_id_for_launch,
                            "activation_ref": CONTENT_ACTIVATION_REF,
                            "declaration_id": CONTENT_DECLARATION_ID,
                        }),
                        launch_context,
                        launch_state,
                    ),
                )
                .unwrap_or_else(|error| panic!("content qualification launch refused: {error:?}"))
        });
    let verifier_thread_id = launch["thread_id"]
        .as_str()
        .expect("content qualification launch returned no exact verifier thread")
        .to_owned();
    wait_for_completed_thread(state, &verifier_thread_id).await;
    let qualification = ryeos_app::operator_external_content::content_qualification::qualify(
        state.clone(),
        context.clone(),
        ryeos_app::operator_external_content::content_qualification::ContentQualificationProofRequest {
            launch: ryeos_app::operator_external_content::content_qualification::ContentQualificationLaunchRequest {
                launch_id,
                activation_ref: CONTENT_ACTIVATION_REF.to_owned(),
                declaration_id: CONTENT_DECLARATION_ID.to_owned(),
            },
            verifier_chain_root_id: verifier_thread_id.clone(),
            verifier_thread_id,
        },
    )
    .await
    .unwrap();
    drop(launch_runtime);
    wait_for_sole_state_owner(state).await;
    let limits = state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy>()
        .unwrap()
        .closure_limits()
        .unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let guard = authority.acquire_shared_guard().unwrap();
    let published = ryeos_state::external_content::qualification_publication::lookup_exact(
        &authority,
        &qualification.coordinate_id,
        &qualification.qualification_hash,
        &context.fingerprint,
        state.identity.verifying_key(),
        limits,
        &guard,
    )
    .unwrap()
    .expect("content qualification is not published at its exact coordinate");
    drop(guard);
    assert_eq!(
        published.evidence.purpose.subject_manifest_hash,
        runtime_manifest_hash
    );
    assert_eq!(
        published.evidence.purpose.policy_source.canonical_ref,
        CONTENT_POLICY_REF
    );
    for claim in required_claims {
        assert!(
            published
                .evidence
                .result
                .claims
                .iter()
                .any(|attested| attested == claim),
            "content qualification did not attest {claim}"
        );
    }
    ActivatedRuntimeQualification {
        activation_ref: CONTENT_ACTIVATION_REF.to_owned(),
        coordinate_id: qualification.coordinate_id,
        attestation_hash: qualification.qualification_hash,
        owner_principal: context.fingerprint.clone(),
        required_claims: required_claims
            .iter()
            .map(|claim| (*claim).to_owned())
            .collect(),
    }
}

async fn wait_for_activation(state: &ryeos_app::state::AppState, job_id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = state
            .state_store
            .with_state_db(|db| db.get_sync_job(job_id))
            .unwrap()
            .expect("managed activation job disappeared");
        match job.state {
            ryeos_state::SyncJobState::Completed => {
                let response: ryeos_api::handlers::external_content_activate::Response =
                    serde_json::from_value(job.result.expect("completed activation has no result"))
                        .unwrap();
                return response
                    .receipt_hash
                    .expect("completed activation has no receipt");
            }
            ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled => {
                panic!(
                    "managed activation ended {}: {}",
                    job.state.as_str(),
                    job.last_error
                        .as_deref()
                        .unwrap_or("no retained diagnostic")
                )
            }
            _ => {}
        }
        assert!(
            Instant::now() < deadline,
            "managed activation did not settle; phase={}",
            job.phase
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// An owner-private directory beside the node root for the offline archive
/// root the node policy admits.
pub fn archive_root(node_root: &Path) -> PathBuf {
    let root = node_root.join("synthetic-archive-root");
    std::fs::create_dir_all(&root).unwrap();
    lillux::PinnedDirectory::open(&root)
        .unwrap()
        .unwrap()
        .tighten_owner_private_directory()
        .unwrap();
    root
}

// ---------------------------------------------------------------------------
// Guest owner runtime product and placement selection
// ---------------------------------------------------------------------------
