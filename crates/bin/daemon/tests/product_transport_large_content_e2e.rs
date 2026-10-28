//! Two-daemon end-to-end proof that a large-content product crosses between
//! nodes and lands resident.
//!
//! ORIGIN publishes a product whose manifest names a file held in its
//! large-object store, together with the witness attestation and the origin
//! admission. HOSTED is configured with a remote entry for ORIGIN (pinned
//! signing key, principal id, site id), a remote-operator grant for ORIGIN's
//! operator bound to ORIGIN's site, and a remote-node grant for ORIGIN's node
//! key. ORIGIN holds a remote entry named `hosted` so the push service can
//! address HOSTED.
//!
//! Scenarios that drive HOSTED directly sign their requests exactly as the
//! origin's configured-operator client does: the operator key signs the
//! request and ORIGIN's node key co-signs it as the forwarding authority.
//!
//! The product's file keeps the store's own sidecar chunk size, because a
//! hosted store admits only manifests whose sidecar chunk size equals its own.
//! The file is therefore a few MiB in a single sidecar chunk: it crosses
//! several 512 KiB wire chunks, but not two sidecar chunks.

use ryeosd::test_support as common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_app::test_support::TestStateStore;
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_state::LargeObjectStore;
use ryeos_state::external_content::products::publication::{
    ProductCaptureCoordinate, publish_product_witness,
};
use ryeos_state::external_content::products::{
    PRODUCT_DECLARATIONS_SCHEMA, ProductBounds, ProductCaptureEvidence, ProductDeclaration,
    ProductDeclarations, ProductProducerAdmission, ProductShape, ProductStorage,
};
use ryeos_state::object_closure::ObjectClosureLimits;
use serde_json::{Value, json};

use common::DaemonHarness;
use common::fast_fixture::FastFixture;

/// Wire chunk length shared by the stage and read services.
const WIRE_CHUNK_BYTES: usize = 512 * 1024;
/// Byte bound every request carries; above the product, below the policy.
const MAXIMUM_BYTES: u64 = 8 * 1024 * 1024;
/// The product's file: six and a half wire chunks plus a few bytes, so the
/// last wire chunk is partial and not aligned to a wire boundary.
const PAYLOAD_BYTES: usize = 3 * 1024 * 1024 + WIRE_CHUNK_BYTES / 2 + 17;
const PRODUCT_ROOT: &str = "T-00000000-0000-0000-0000-000000000001";
const PRODUCT_TERMINAL: &str = "T-00000000-0000-0000-0000-000000000002";
const HOSTED_REMOTE: &str = "hosted";
const ORIGIN_REMOTE: &str = "origin";

/// Capabilities granted to the origin operator and node on HOSTED.
const PUSH_SCOPES: &[&str] = &[
    "ryeos.execute.service.external-content/product-offer",
    "ryeos.execute.service.external-content/product-accept",
    "ryeos.execute.service.objects/stage-chunk",
];

/// What the origin NODE's grant on HOSTED carries: the push capabilities plus
/// the authority to co-sign forwarded operator requests, without which the
/// hosted node refuses the operator's forwarding proof.
const ORIGIN_NODE_SCOPES: &[&str] = &[
    "ryeos.execute.service.external-content/product-offer",
    "ryeos.execute.service.external-content/product-accept",
    "ryeos.execute.service.objects/stage-chunk",
    ryeos_node_runtime::identity::FORWARDED_OPERATOR_ATTESTATION_SCOPE,
];

/// Capabilities a pulling node needs on ORIGIN to read a closure, its
/// admission and the large-object chunks.
const PULL_SCOPES: &[&str] = &[
    "ryeos.execute.service.objects/get",
    "ryeos.execute.service.objects/has",
    "ryeos.execute.service.objects/closure/get",
    "ryeos.execute.service.objects/closure/describe",
    "ryeos.execute.service.admission/attestations-for-subject",
    "ryeos.execute.service.admission/status",
];

fn hosted_node_key() -> SigningKey {
    SigningKey::from_bytes(&[71_u8; 32])
}

fn hosted_operator_key() -> SigningKey {
    SigningKey::from_bytes(&[72_u8; 32])
}

/// A second remote operator, admitted on HOSTED for ORIGIN's site, standing
/// for a different principal speaking for the same origin.
fn second_operator_key() -> SigningKey {
    SigningKey::from_bytes(&[73_u8; 32])
}

fn puller_node_key() -> SigningKey {
    SigningKey::from_bytes(&[74_u8; 32])
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn site_of(node_key: &SigningKey) -> String {
    format!("site:{}", fingerprint(node_key))
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
        "product-transport-peer",
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
        "product-transport-operator",
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

/// Deterministic incompressible bytes.
fn large_payload() -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut bytes = Vec::with_capacity(PAYLOAD_BYTES);
    while bytes.len() < PAYLOAD_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(PAYLOAD_BYTES);
    bytes
}

/// What ORIGIN published, and the bytes behind it.
struct Published {
    witness_hash: String,
    admission_hash: String,
    manifest_hash: String,
    file_sha256: String,
    size: u64,
    chunk_size: u64,
    chunk_hashes: Vec<String>,
    payload: Vec<u8>,
}

/// Ingest `payload` into the stopped daemon's large-object store, then publish
/// its manifest, a node-signed witness owned by `owner_principal`, and the
/// node's admission of the witness. Synchronous so the store owner never
/// crosses an await.
fn publish_large_product(
    state_path: &Path,
    scratch: &Path,
    payload: &[u8],
    owner_principal: &str,
) -> Result<Published> {
    let identity = daemon_identity(state_path)?;
    let signer = NodeIdentitySigner::from_identity(&identity);
    let store = open_daemon_state(state_path)?;
    let authority = store.pinned_state_authority()?;

    let large = LargeObjectStore::open_or_create_under(authority.runtime_directory())?;
    std::fs::create_dir_all(scratch)?;
    std::fs::write(scratch.join("payload.bin"), payload)?;
    let directory = lillux::PinnedDirectory::open(scratch)?.context("open payload directory")?;
    let file = directory
        .open_regular(std::ffi::OsStr::new("payload.bin"), false)?
        .context("open payload file")?;
    let source_identity = ryeos_state::PinnedLargeObjectSourceIdentity {
        file: lillux::pinned_regular_file_identity(&file)?,
        size: u64::try_from(payload.len())?,
    };
    let ingested = large.ingest_open_regular(file, source_identity, "payload.bin", None)?;
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
    let attestation = evidence.sign_attestation(&signer, lillux::time::iso8601_now())?;
    let witness_hash = publish_product_witness(
        &authority,
        &ProductCaptureCoordinate::from_evidence(&evidence)?,
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
        file_sha256: ingested.file_sha256,
        size: ingested.size,
        chunk_size: ingested.chunk_size,
        chunk_hashes: ingested.chunk_hashes,
        payload: payload.to_vec(),
    })
}

/// The sidecar HOSTED holds for `file_sha256`, read from its stopped store.
fn read_sidecar(
    state_path: &Path,
    file_sha256: &str,
) -> Result<ryeos_state::large_object_store::LargeObjectSidecar> {
    let store = open_daemon_state(state_path)?;
    let authority = store.pinned_state_authority()?;
    let large = LargeObjectStore::open_under(authority.runtime_directory())?
        .context("hosted node has no large-object store")?;
    large
        .sidecar(file_sha256)?
        .context("hosted node holds no sidecar for the object")
}

/// ORIGIN with a published product.
struct Origin {
    harness: DaemonHarness,
    fixture: FastFixture,
    product: Published,
    _scratch: tempfile::TempDir,
}

/// Start ORIGIN on the standard fixture keys, publish the product while the
/// daemon is stopped (a running daemon owns its store), optionally admit a
/// pulling node, and bring it back up.
async fn start_origin(pulling_node: Option<&SigningKey>) -> Result<Origin> {
    let (mut harness, fixture) = DaemonHarness::start_fast_with(|_, _, _| Ok(()), |_| {}).await?;
    harness.kill_daemon().await?;
    if let Some(node) = pulling_node {
        authorize_remote_node(
            &harness.state_path,
            &fixture,
            node,
            &site_of(node),
            PULL_SCOPES,
        )?;
    }
    let scratch = tempfile::tempdir()?;
    let product = publish_large_product(
        &harness.state_path,
        &scratch.path().join("payload"),
        &large_payload(),
        &format!("fp:{}", fixture.user_fp()),
    )?;
    harness.respawn_with(|_| {}).await?;
    Ok(Origin {
        harness,
        fixture,
        product,
        _scratch: scratch,
    })
}

/// ORIGIN plus the node that receives its product.
struct Pair {
    origin: Origin,
    hosted: DaemonHarness,
    hosted_fixture: FastFixture,
}

async fn start_pair() -> Result<Pair> {
    let origin_node = common::fast_fixture::node_signing_key();
    let origin_operator = common::fast_fixture::user_signing_key();
    let origin_site = site_of(&origin_node);
    let second = second_operator_key();
    let (hosted, hosted_fixture) = DaemonHarness::start_fast_with_node_and_user_keys(
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
            for operator in [&origin_operator, &second] {
                authorize_remote_operator(
                    state_path,
                    fixture,
                    operator,
                    &origin_site,
                    PUSH_SCOPES,
                )?;
            }
            Ok(())
        },
        |_| {},
    )
    .await?;
    let origin = start_origin(None).await?;
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
    Ok(Pair {
        origin,
        hosted,
        hosted_fixture,
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

    /// POST and require success, returning the parsed body.
    async fn post_ok(&self, path: &str, body: &Value) -> Result<Value> {
        let (status, text) = self.post(path, body).await?;
        ensure!(status.is_success(), "{path} returned {status}: {text}");
        serde_json::from_str(&text).with_context(|| format!("{path} returned non-JSON: {text}"))
    }
}

impl Pair {
    fn client(&self, operator: SigningKey) -> HostedClient {
        HostedClient {
            operator,
            origin_node: common::fast_fixture::node_signing_key(),
            origin_site_id: self.origin.fixture.site_id(),
            audience: format!("fp:{}", self.hosted_fixture.node_fp()),
            base: format!("http://{}", self.hosted.bind),
            http: reqwest::Client::new(),
        }
    }

    fn origin_operator_client(&self) -> HostedClient {
        self.client(common::fast_fixture::user_signing_key())
    }

    /// The origin admission record for the witness, as the origin serves it.
    async fn admission_record(&self) -> Result<Value> {
        let product = &self.origin.product;
        let (status, body) = self
            .origin
            .harness
            .post_json(
                "/admission/attestations-for-subject",
                json!({
                    "subject_hash": product.witness_hash,
                    "policy": ryeos_state::admission::LOCAL_ADMISSION_POLICY,
                }),
            )
            .await?;
        ensure!(
            status.is_success(),
            "origin attestations-for-subject returned {status}: {body}"
        );
        body["attestations"]
            .as_array()
            .context("origin returned no attestation list")?
            .iter()
            .find(|record| record["attestation_hash"] == product.admission_hash)
            .cloned()
            .context("origin does not list its own admission of the witness")
    }

    /// Canonical objects of the product closure, read from ORIGIN.
    async fn origin_objects(&self, hashes: &[String]) -> Result<Vec<Value>> {
        let (status, body) = self
            .origin
            .harness
            .post_json(
                "/objects/get",
                json!({"object_hashes": hashes, "blob_hashes": []}),
            )
            .await?;
        ensure!(
            status.is_success(),
            "origin objects/get returned {status}: {body}"
        );
        let entries = body["entries"]
            .as_array()
            .context("origin objects/get returned no entries")?;
        let mut objects = Vec::with_capacity(entries.len());
        for entry in entries {
            ensure!(
                entry["kind"] == "object",
                "origin does not hold object {}",
                entry["hash"]
            );
            objects.push(entry["value"].clone());
        }
        Ok(objects)
    }

    fn offer_body(&self, record: &Value, objects: Vec<Value>, staging_id: Option<&str>) -> Value {
        let product = &self.origin.product;
        let mut body = json!({
            "witness_hash": product.witness_hash,
            "origin_admission_hash": product.admission_hash,
            "maximum_bytes": MAXIMUM_BYTES,
            "origin_admission": record,
            "objects": objects,
        });
        if let Some(staging_id) = staging_id {
            body["staging_id"] = json!(staging_id);
        }
        body
    }

    /// Offer the whole canonical closure, answering each `missing_objects`
    /// list from ORIGIN until none remains. Returns the staging capability
    /// and the last offer answer.
    async fn offer_closure(
        &self,
        client: &HostedClient,
        record: &Value,
    ) -> Result<(String, Value)> {
        let mut wanted = vec![self.origin.product.witness_hash.clone()];
        let mut staging_id: Option<String> = None;
        for _ in 0..16 {
            let objects = self.origin_objects(&wanted).await?;
            let answer = client
                .post_ok(
                    "/external-content/product-offer",
                    &self.offer_body(record, objects, staging_id.as_deref()),
                )
                .await?;
            staging_id = Some(
                answer["staging_id"]
                    .as_str()
                    .context("offer answered without a staging capability")?
                    .to_owned(),
            );
            let missing = answer["missing_objects"]
                .as_array()
                .context("offer answered without a missing list")?;
            if missing.is_empty() {
                return Ok((staging_id.context("staging capability")?, answer));
            }
            wanted = missing
                .iter()
                .map(|hash| hash.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .context("offer listed a non-string hash")?;
        }
        bail!("hosted node still lacks closure objects after 16 offers")
    }

    /// Re-offer with no new objects to read HOSTED's progress.
    async fn offer_progress(
        &self,
        client: &HostedClient,
        record: &Value,
        staging_id: &str,
    ) -> Result<Value> {
        client
            .post_ok(
                "/external-content/product-offer",
                &self.offer_body(record, Vec::new(), Some(staging_id)),
            )
            .await
    }

    fn stage_body(&self, staging_id: &str, offset: usize, data: &[u8]) -> Value {
        let product = &self.origin.product;
        json!({
            "staging_id": staging_id,
            "namespace": "large_object",
            "hash": product.file_sha256,
            "total_size": product.size,
            "offset": offset,
            "data": base64::engine::general_purpose::STANDARD.encode(data),
            "manifest_hash": product.manifest_hash,
        })
    }

    /// Stage the wire chunks covering `from..to` of the payload and return the
    /// last answer. `from` is a wire-chunk boundary.
    async fn send_range(
        &self,
        client: &HostedClient,
        staging_id: &str,
        from: usize,
        to: usize,
    ) -> Result<Value> {
        let payload = &self.origin.product.payload;
        let mut offset = from;
        let mut last = Value::Null;
        while offset < to {
            let end = (offset + WIRE_CHUNK_BYTES).min(to);
            last = client
                .post_ok(
                    "/objects/stage-chunk",
                    &self.stage_body(staging_id, offset, &payload[offset..end]),
                )
                .await?;
            offset = end;
        }
        Ok(last)
    }

    async fn accept(
        &self,
        client: &HostedClient,
        staging_id: &str,
    ) -> Result<(reqwest::StatusCode, String)> {
        let product = &self.origin.product;
        client
            .post(
                "/external-content/product-accept",
                &json!({
                    "staging_id": staging_id,
                    "witness_hash": product.witness_hash,
                    "origin_admission_hash": product.admission_hash,
                    "maximum_bytes": MAXIMUM_BYTES,
                }),
            )
            .await
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
}

/// The product's file as a node serves it through `objects/get`, reassembled
/// from wire chunks.
async fn read_resident_file(node: &DaemonHarness, product: &Published) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        let (status, body) = node
            .post_json(
                "/objects/get",
                json!({
                    "object_hashes": [],
                    "blob_hashes": [],
                    "large_object_chunk": {
                        "hash": product.file_sha256,
                        "offset": bytes.len(),
                        "length": WIRE_CHUNK_BYTES,
                    },
                }),
            )
            .await?;
        ensure!(
            status.is_success(),
            "large-object read returned {status}: {body}"
        );
        let chunk = &body["large_object_chunk"];
        ensure!(
            chunk["kind"] == "large_object_chunk",
            "node does not hold the large object: {chunk}"
        );
        bytes.extend(
            base64::engine::general_purpose::STANDARD
                .decode(chunk["data"].as_str().context("chunk carries no data")?)?,
        );
        if chunk["eof"] == true {
            return Ok(bytes);
        }
    }
}

async fn assert_resident_and_exact(node: &DaemonHarness, product: &Published) -> Result<()> {
    let bytes = read_resident_file(node, product).await?;
    assert_eq!(u64::try_from(bytes.len())?, product.size);
    assert_eq!(lillux::sha256_hex(&bytes), product.file_sha256);
    assert!(
        bytes == product.payload,
        "resident bytes differ from the origin file"
    );
    Ok(())
}

fn assert_sidecar_is_the_manifest_trail(
    sidecar: &ryeos_state::large_object_store::LargeObjectSidecar,
    product: &Published,
) {
    assert_eq!(sidecar.file_sha256, product.file_sha256);
    assert_eq!(sidecar.size, product.size);
    assert_eq!(sidecar.chunk_size, product.chunk_size);
    assert_eq!(sidecar.chunk_hashes, product.chunk_hashes);
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

#[tokio::test(flavor = "multi_thread")]
async fn push_crosses_and_lands_resident_with_the_manifest_sidecar() -> Result<()> {
    let mut pair = start_pair().await?;
    let outcome = pair.push().await?;
    assert_hash(&outcome["acceptance_hash"], "acceptance_hash");
    assert_eq!(outcome["reused_existing"], false);
    assert_eq!(outcome["large_objects_pushed"], 1);
    assert_eq!(outcome["bytes_pushed"], pair.origin.product.size);
    assert_eq!(outcome["manifest_hash"], pair.origin.product.manifest_hash);
    assert_eq!(outcome["witness_hash"], pair.origin.product.witness_hash);

    assert_resident_and_exact(&pair.hosted, &pair.origin.product).await?;
    pair.hosted.kill_daemon().await?;
    let sidecar = read_sidecar(&pair.hosted.state_path, &pair.origin.product.file_sha256)?;
    assert_sidecar_is_the_manifest_trail(&sidecar, &pair.origin.product);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_transfer_resumes_from_the_staged_bytes() -> Result<()> {
    let pair = start_pair().await?;
    let product = &pair.origin.product;
    let client = pair.origin_operator_client();
    let record = pair.admission_record().await?;

    let (staging_id, first) = pair.offer_closure(&client, &record).await?;
    let large = &first["large_objects"][0];
    assert_eq!(large["file_sha256"], product.file_sha256);
    assert_eq!(large["size"], product.size);
    assert_eq!(large["chunk_size"], product.chunk_size);
    assert_eq!(large["staged_bytes"], 0);
    assert_eq!(large["resident"], false);
    assert_eq!(first["ready"], false);

    let sent = 3 * WIRE_CHUNK_BYTES;
    let partial = pair.send_range(&client, &staging_id, 0, sent).await?;
    assert_eq!(partial["staged_bytes"], sent);
    assert_eq!(partial["complete"], false);

    let resumed = pair.offer_progress(&client, &record, &staging_id).await?;
    assert_eq!(resumed["staging_id"], staging_id.as_str());
    assert_eq!(resumed["large_objects"][0]["staged_bytes"], sent);
    assert_eq!(resumed["large_objects"][0]["resident"], false);
    assert_eq!(resumed["ready"], false);

    let finished = pair
        .send_range(&client, &staging_id, sent, product.payload.len())
        .await?;
    assert_eq!(finished["complete"], true);
    assert_eq!(finished["staged_bytes"], product.size);
    let ready = pair.offer_progress(&client, &record, &staging_id).await?;
    assert_eq!(ready["large_objects"][0]["resident"], true);
    assert_eq!(ready["ready"], true);

    let (status, text) = pair.accept(&client, &staging_id).await?;
    ensure!(status.is_success(), "accept returned {status}: {text}");
    let accepted: Value = serde_json::from_str(&text)?;
    let acceptance = assert_hash(&accepted["acceptance_hash"], "acceptance_hash");
    assert_eq!(accepted["reused_existing"], false);
    assert_resident_and_exact(&pair.hosted, product).await?;

    // The origin's own push of the same product finds everything resident and
    // arrives at the acceptance the manual drive recorded.
    let pushed = pair.push().await?;
    assert_eq!(pushed["acceptance_hash"], acceptance.as_str());
    assert_eq!(pushed["reused_existing"], true);
    assert_eq!(pushed["bytes_pushed"], 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_accept_returns_the_recorded_acceptance() -> Result<()> {
    let pair = start_pair().await?;
    let outcome = pair.push().await?;
    let acceptance = assert_hash(&outcome["acceptance_hash"], "acceptance_hash");
    let staging_id = outcome["staging_id"]
        .as_str()
        .context("push outcome carries no staging capability")?
        .to_owned();

    let client = pair.origin_operator_client();
    let (status, text) = pair.accept(&client, &staging_id).await?;
    ensure!(
        status.is_success(),
        "second accept returned {status}: {text}"
    );
    let again: Value = serde_json::from_str(&text)?;
    assert_eq!(again["acceptance_hash"], acceptance.as_str());
    assert_eq!(again["reused_existing"], true);
    assert_eq!(again["witness_hash"], pair.origin.product.witness_hash);
    assert_eq!(again["manifest_hash"], pair.origin.product.manifest_hash);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_refuses_forged_misowned_and_incomplete_offers() -> Result<()> {
    let pair = start_pair().await?;
    let product = &pair.origin.product;
    let client = pair.origin_operator_client();
    let record = pair.admission_record().await?;

    // An admission signed by a key that is not the pinned origin key is
    // refused before anything is stored.
    {
        let scratch = tempfile::tempdir()?;
        let rogue = NodeIdentity::create(&scratch.path().join("rogue.pem"))?;
        let rogue_signer = NodeIdentitySigner::from_identity(&rogue);
        let forged = ryeos_state::Attestation::unsigned(
            product.witness_hash.clone(),
            "accepted".to_owned(),
            ryeos_state::admission::LOCAL_ADMISSION_POLICY.to_owned(),
            record["issued_at"]
                .as_str()
                .context("admission record has no issued_at")?
                .to_owned(),
            None,
            record["attestation"]["evidence"].clone(),
        )
        .sign(&rogue_signer)?
        .to_value();
        let forged_hash = lillux::sha256_hex(lillux::canonical_json(&forged)?.as_bytes());
        let mut forged_record = record.clone();
        forged_record["attestation_hash"] = json!(forged_hash);
        forged_record["issuer"] = forged["issuer"].clone();
        forged_record["attestation"] = forged;
        let mut body = pair.offer_body(&forged_record, Vec::new(), None);
        body["origin_admission_hash"] = json!(forged_hash);
        let (status, text) = client
            .post("/external-content/product-offer", &body)
            .await?;
        assert_refused(
            status,
            &text,
            "remote admission attestation issuer mismatch",
            "admission signed by an unpinned key",
        );
    }

    // The honest offer opens the stage the remaining refusals address.
    let (staging_id, _) = pair.offer_closure(&client, &record).await?;

    // A chunk past the staged length is refused with the expected offset.
    let out_of_order = 2 * WIRE_CHUNK_BYTES;
    let (status, text) = client
        .post(
            "/objects/stage-chunk",
            &pair.stage_body(
                &staging_id,
                out_of_order,
                &product.payload[out_of_order..out_of_order + WIRE_CHUNK_BYTES],
            ),
        )
        .await?;
    assert_refused(
        status,
        &text,
        &format!("large-object chunk is out of order: expected offset 0, got {out_of_order}"),
        "chunk past the staged length",
    );

    // A manifest the stage does not protect is refused.
    let mut unprotected = pair.stage_body(&staging_id, 0, &product.payload[..WIRE_CHUNK_BYTES]);
    unprotected["manifest_hash"] = json!("a".repeat(64));
    let (status, text) = client.post("/objects/stage-chunk", &unprotected).await?;
    assert_refused(
        status,
        &text,
        "was not uploaded under this staging capability",
        "manifest the stage does not protect",
    );

    // Another principal cannot reopen the stage.
    let stranger = pair.client(second_operator_key());
    let (status, text) = stranger
        .post(
            "/objects/stage-chunk",
            &pair.stage_body(&staging_id, 0, &product.payload[..WIRE_CHUNK_BYTES]),
        )
        .await?;
    assert_refused(
        status,
        &text,
        "durable CAS upload capability belongs to another principal",
        "stage reopened by a different owner",
    );

    // Accepting while the large object is still absent is refused.
    let (status, text) = pair.accept(&client, &staging_id).await?;
    assert_refused(status, &text, "is absent", "accept before the large object");

    // A tampered chunk is refused where the sidecar chunk completes, and the
    // staged bytes are cut back so the sender restarts at its first byte.
    let last_start = (product.payload.len() / WIRE_CHUNK_BYTES) * WIRE_CHUNK_BYTES;
    pair.send_range(&client, &staging_id, 0, last_start).await?;
    let mut tampered = product.payload[last_start..].to_vec();
    tampered[0] ^= 0x01;
    let (status, text) = client
        .post(
            "/objects/stage-chunk",
            &pair.stage_body(&staging_id, last_start, &tampered),
        )
        .await?;
    assert_refused(
        status,
        &text,
        "chunk 0 of the large object failed its hash",
        "tampered chunk",
    );
    let after = pair.offer_progress(&client, &record, &staging_id).await?;
    assert_eq!(after["large_objects"][0]["staged_bytes"], 0);
    assert_eq!(after["large_objects"][0]["resident"], false);

    // The honest bytes then complete and are accepted.
    let finished = pair
        .send_range(&client, &staging_id, 0, product.payload.len())
        .await?;
    assert_eq!(finished["complete"], true);
    let (status, text) = pair.accept(&client, &staging_id).await?;
    ensure!(
        status.is_success(),
        "honest accept returned {status}: {text}"
    );

    // A different remote operator may offer the same product, but the witness
    // belongs to ORIGIN's operator, so its acceptance is refused.
    let (other_stage, _) = pair.offer_closure(&stranger, &record).await?;
    assert_ne!(other_stage, staging_id);
    let (status, text) = pair.accept(&stranger, &other_stage).await?;
    assert_refused(
        status,
        &text,
        "same configured operator",
        "offer from a different operator principal",
    );
    Ok(())
}

/// Pull form. HOSTED's counterpart is a node whose local operator is ORIGIN's
/// operator, since a pulled acceptance is owned by the local configured
/// operator.
#[tokio::test(flavor = "multi_thread")]
async fn receive_product_pulls_the_large_object() -> Result<()> {
    let puller_key = puller_node_key();
    let origin = start_origin(Some(&puller_key)).await?;
    let (puller, _puller_fixture) =
        DaemonHarness::start_fast_with_node_key(puller_key.clone(), |_, _, _| Ok(()), |_| {})
            .await?;
    install_single_remote(
        &puller.state_path,
        remote_config(
            ORIGIN_REMOTE,
            format!("http://{}", origin.harness.bind),
            &common::fast_fixture::node_signing_key(),
            &origin.fixture.site_id(),
        ),
    )?;
    let product = &origin.product;
    let (status, body) = puller
        .post_execute(
            "service:external-content/receive-product",
            ".",
            json!({
                "remote": ORIGIN_REMOTE,
                "witness_hash": product.witness_hash,
                "origin_admission_hash": product.admission_hash,
                "maximum_bytes": MAXIMUM_BYTES,
            }),
        )
        .await?;
    ensure!(
        status.is_success(),
        "receive-product returned {status}: {body}"
    );
    let received = &body["result"];
    assert_hash(&received["acceptance_hash"], "acceptance_hash");
    assert_eq!(received["reused_existing"], false);
    assert_eq!(received["witness_hash"], product.witness_hash);
    assert_eq!(received["manifest_hash"], product.manifest_hash);
    assert_resident_and_exact(&puller, product).await?;
    Ok(())
}
