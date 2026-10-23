//! Offer one admitted product witness closure to this node (the push form).
//!
//! The origin node signs in as an admitted remote operator. Before anything
//! is stored, the offered origin admission is verified exactly as the pull
//! path verifies a fetched one: against the pinned key of the remote entry
//! configured for the authenticated origin site. Canonical objects are then
//! stored under a durable, operator-owned upload stage that protects the
//! closure and every large-object hash, and the response names what is still
//! missing so the origin can resume.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_node_runtime::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use ryeos_node_runtime::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::AdmissionAttestationRemoteRecord;
use crate::remote::config::{RemoteConfig, resolve_remote_by_site_id};
use crate::remote::import::verify_remote_attestation_record;

/// Canonical objects accepted per call. A closure larger than this is offered
/// across several calls; the stage accumulates.
pub const MAX_OFFERED_OBJECTS: usize = 4096;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub witness_hash: String,
    pub origin_admission_hash: String,
    pub maximum_bytes: u64,
    /// The origin's complete admission attestation record for the witness.
    pub origin_admission: Value,
    pub objects: Vec<Value>,
    #[serde(default)]
    pub staging_id: Option<String>,
}

impl Request {
    fn validate(&self) -> Result<()> {
        for hash in [&self.witness_hash, &self.origin_admission_hash] {
            if !is_canonical_hash(hash) {
                bail!("product offer requires exact canonical witness and admission hashes");
            }
        }
        if self.maximum_bytes == 0 {
            bail!("product offer requires a positive byte bound");
        }
        if self.objects.len() > MAX_OFFERED_OBJECTS {
            bail!(
                "product offer carries {} objects; the per-call bound is {MAX_OFFERED_OBJECTS}",
                self.objects.len()
            );
        }
        if self
            .staging_id
            .as_deref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
        {
            bail!("product offer staging_id is not a server-issued capability");
        }
        Ok(())
    }
}

pub(crate) fn is_canonical_hash(hash: &str) -> bool {
    lillux::valid_hash(hash) && !hash.bytes().any(|byte| byte.is_ascii_uppercase())
}

/// The authenticated pushing operator and the origin it speaks for. Built
/// only by [`authenticate_push_origin`]; no field comes from the request.
pub(crate) struct PushAuthority {
    /// Principal that owns the upload stage and must own the witness.
    pub owner_principal: String,
    pub origin_key: lillux::crypto::VerifyingKey,
    pub expected_issuer: String,
}

/// Authenticate a push-form caller, in order: admitted operator class, the
/// operator's current node-signed grant, then the origin resolved solely from
/// the authenticated origin site through the layered remotes.
pub(crate) fn authenticate_push_origin(
    state: &AppState,
    ctx: &HandlerContext,
) -> Result<PushAuthority> {
    ryeos_app::operator_authority::require_admitted_operator(state, ctx)?;
    let authority = ryeos_app::operator_authority::admitted_operator_authority_for_principal(
        state,
        &ctx.fingerprint,
    )?;
    let site_id = ctx
        .authenticated_origin_site_id
        .as_deref()
        .context("product push requires an authenticated origin site")?;
    if authority.origin_site_id != site_id {
        bail!(
            "operator grant is bound to origin site {}, not the authenticated site {site_id}",
            authority.origin_site_id
        );
    }
    let remotes = crate::remote::config::load_remotes_layered(&state.config.app_root, None)?;
    let (origin_key, expected_issuer) = resolve_push_origin(&remotes, site_id)?;
    Ok(PushAuthority {
        owner_principal: ctx.fingerprint.clone(),
        origin_key,
        expected_issuer,
    })
}

/// Resolve the configured remote for an authenticated origin site to its
/// pinned key and the issuer string its admissions must carry.
fn resolve_push_origin(
    remotes: &std::collections::HashMap<String, RemoteConfig>,
    site_id: &str,
) -> Result<(lillux::crypto::VerifyingKey, String)> {
    let resolved = resolve_remote_by_site_id(remotes, site_id)
        .with_context(|| format!("no configured remote for origin site {site_id}"))?;
    let key = resolved
        .remote
        .pinned_signing_key()
        .context("product origin has no pinned signing key")?;
    let issuer = format!("fp:{}", lillux::crypto::fingerprint(&key));
    Ok((key, issuer))
}

/// Verify the offered admission record exactly as the pull path verifies a
/// fetched one, and bind it to the named witness and admission hash.
fn verify_offered_admission(
    req: &Request,
    authority: &PushAuthority,
) -> Result<AdmissionAttestationRemoteRecord> {
    let record: AdmissionAttestationRemoteRecord =
        serde_json::from_value(req.origin_admission.clone())
            .context("offered origin admission is not an admission attestation record")?;
    if record.attestation_hash != req.origin_admission_hash {
        bail!(
            "offered origin admission is {}, not the named {}",
            record.attestation_hash,
            req.origin_admission_hash
        );
    }
    verify_remote_attestation_record(
        &record,
        &req.witness_hash,
        ryeos_state::admission::LOCAL_ADMISSION_POLICY,
        &authority.expected_issuer,
        &authority.origin_key,
    )?;
    Ok(record)
}

/// Canonical identity of one push: the same witness and admission offered by
/// the same operator always address the same durable stage contract.
pub(crate) fn stage_request_digest(
    witness_hash: &str,
    origin_admission_hash: &str,
    owner_principal: &str,
) -> Result<String> {
    let canonical = lillux::canonical_json(&serde_json::json!({
        "kind": "product-push",
        "witness_hash": witness_hash,
        "origin_admission_hash": origin_admission_hash,
        "owner_principal": owner_principal,
    }))?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let authority = authenticate_push_origin(&state, &ctx)?;
    req.validate()?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if req.maximum_bytes > policy.limits.max_total_bytes {
        bail!("product offer byte bound exceeds current import policy");
    }
    // Nothing is stored until the admission is proven.
    let record = verify_offered_admission(&req, &authority)?;
    tokio::task::spawn_blocking(move || stage_offer(&state, &authority, &req, &record))
        .await
        .context("product offer task stopped")?
}

fn stage_offer(
    state: &AppState,
    authority: &PushAuthority,
    req: &Request,
    record: &AdmissionAttestationRemoteRecord,
) -> Result<Value> {
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    let limits = state
        .node_policy
        .require::<NodeObjectClosurePolicy>()?
        .closure_limits()?;
    let object_ceiling =
        usize::try_from(limits.max_object_bytes).context("object ceiling exceeds this platform")?;
    let pinned = state.state_store.pinned_state_authority()?;
    let guard = pinned.acquire_shared_guard()?;
    let cas = pinned.cas_store()?;
    let large_store = pinned.large_object_store()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire product offer write permit: {error}"))?;
    let recovery = pinned.require_recovery()?;
    let digest = stage_request_digest(
        &req.witness_hash,
        &req.origin_admission_hash,
        &authority.owner_principal,
    )?;
    let publication_key = ryeos_state::DurableCasPublicationKey::external_content_import(&digest)?;
    let mut stage = match req.staging_id.as_deref() {
        Some(staging_id) => {
            let stage = recovery.open_durable_cas_upload_admitted(
                &guard,
                staging_id,
                &authority.owner_principal,
            )?;
            stage.ensure_publication_contract(&publication_key, None)?;
            stage
        }
        None => recovery.begin_durable_cas_upload_admitted(
            &guard,
            &authority.owner_principal,
            "product-offer",
            &publication_key,
            None,
        )?,
    };
    if let Some(target) = stage.admitted_target_hash() {
        bail!("durable upload stage is already admitted for publication target {target}");
    }

    // The verified admission object itself, then the offered closure objects.
    let admission_hash = stage.store_object(&guard, &cas, &record.attestation)?;
    if admission_hash != req.origin_admission_hash {
        bail!("stored origin admission {admission_hash} differs from the named hash");
    }
    for object in &req.objects {
        lillux::canonical_json_bounded(object, object_ceiling)
            .context("offered object is not bounded canonical JSON")?;
        stage.store_object(&guard, &cas, object)?;
    }

    let report = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [req.witness_hash.clone()],
        limits,
    )?;
    if !report.malformed_objects.is_empty() || !report.unsupported_objects.is_empty() {
        bail!(
            "offered closure has {} malformed and {} unsupported objects",
            report.malformed_objects.len(),
            report.unsupported_objects.len()
        );
    }
    stage.protect_cas_closure(
        &guard,
        report.object_hashes.iter().map(String::as_str),
        report.blob_hashes.iter().map(String::as_str),
    )?;

    // Every large object the present manifests declare, with its sealed size
    // and sidecar chunk size.
    let missing_objects = report
        .missing_objects
        .iter()
        .map(|missing| missing.hash.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut declared = BTreeMap::<String, (u64, u64)>::new();
    for hash in &report.object_hashes {
        if missing_objects.contains(hash.as_str()) {
            continue;
        }
        let Some(value) = cas.get_object(hash)? else {
            continue;
        };
        if value.get("kind").and_then(Value::as_str)
            != Some(ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND)
        {
            continue;
        }
        let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(&value)
            .with_context(|| format!("large-content manifest {hash} is invalid"))?;
        for entry in &manifest.entries {
            if let (Some(file_sha256), Some(size), Some(chunk_size)) =
                (&entry.file_sha256, entry.size, entry.chunk_size)
                && declared
                    .insert(file_sha256.clone(), (size, chunk_size))
                    .is_some_and(|prior| prior != (size, chunk_size))
            {
                bail!("manifests declare conflicting sealed shapes for large object {file_sha256}");
            }
        }
    }
    let mut large_objects = Vec::with_capacity(report.large_object_hashes.len());
    let mut declared_bytes = 0_u64;
    for file_sha256 in &report.large_object_hashes {
        let (size, chunk_size) = declared.get(file_sha256).copied().with_context(|| {
            format!("closure names large object {file_sha256} that no present manifest declares")
        })?;
        declared_bytes = declared_bytes
            .checked_add(size)
            .context("offered large-object byte total overflows")?;
        stage.protect_large_object_hash(&guard, file_sha256)?;
        // An object present without its sidecar was interrupted between the
        // two publications; it is reported non-resident so the push resends it
        // and the chunk store repairs the sidecar.
        let resident = match (
            large_store.object_size(file_sha256)?,
            large_store.sidecar(file_sha256)?,
        ) {
            (Some(_), Some(_)) => {
                large_store.verify_resident_object(file_sha256, size)?;
                true
            }
            _ => false,
        };
        let staged_bytes = if resident {
            size
        } else {
            large_store.staged_bytes(file_sha256)?
        };
        large_objects.push(serde_json::json!({
            "file_sha256": file_sha256,
            "size": size,
            "chunk_size": chunk_size,
            "staged_bytes": staged_bytes,
            "resident": resident,
        }));
    }
    if declared_bytes > req.maximum_bytes {
        bail!(
            "offered large objects total {declared_bytes} bytes, over the {} byte bound",
            req.maximum_bytes
        );
    }
    if declared_bytes > policy.limits.max_total_bytes {
        bail!("offered large objects exceed the current import policy total");
    }

    let missing_blobs = report
        .missing_blobs
        .iter()
        .map(|missing| missing.hash.clone())
        .collect::<Vec<_>>();
    let missing_object_hashes = report
        .missing_objects
        .iter()
        .map(|missing| missing.hash.clone())
        .collect::<Vec<_>>();
    let pending_large = large_objects
        .iter()
        .any(|large| large["resident"] == Value::Bool(false));
    let ready = missing_object_hashes.is_empty() && missing_blobs.is_empty() && !pending_large;
    Ok(serde_json::json!({
        "staging_id": stage.staging_id(),
        "missing_objects": missing_object_hashes,
        "missing_blobs": missing_blobs,
        "large_objects": large_objects,
        "ready": ready,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/product-offer",
    endpoint: "external-content.product-offer",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/product-offer"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { handle(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_service_schema_matches_the_typed_request() {
        let descriptor: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/external-content/product-offer.yaml"
        )))
        .unwrap();
        assert_eq!(descriptor["endpoint"], DESCRIPTOR.endpoint);
        assert_eq!(
            descriptor["required_caps"],
            serde_json::json!(DESCRIPTOR.required_caps)
        );
        assert_eq!(
            descriptor["schema"],
            serde_json::json!({
                "witness_hash": "string",
                "origin_admission_hash": "string",
                "maximum_bytes": "integer",
                "origin_admission": "object",
                "objects": "object[]",
                "staging_id": "string?",
            })
        );
        // Every authored field is a field of the typed request: the request
        // refuses unknown fields, so a field missing from it would fail here.
        let mut coordinates = serde_json::Map::new();
        for (field, kind) in descriptor["schema"].as_object().unwrap() {
            let kind = kind.as_str().unwrap();
            if kind.ends_with('?') {
                continue;
            }
            let value = match kind.trim_end_matches('?') {
                "integer" => serde_json::json!(1),
                "object" => serde_json::json!({}),
                "object[]" => serde_json::json!([]),
                _ => serde_json::json!("x"),
            };
            coordinates.insert(field.clone(), value);
        }
        let mut with_unknown = coordinates.clone();
        with_unknown.insert("not_a_field".to_owned(), serde_json::json!(1));
        assert!(serde_json::from_value::<Request>(serde_json::Value::Object(coordinates)).is_ok());
        assert!(
            serde_json::from_value::<Request>(serde_json::Value::Object(with_unknown)).is_err()
        );
    }
    use ryeos_node_runtime::identity::AuthorizedKeyPrincipalClass;
    use ryeos_state::Signer;
    use std::collections::HashMap;

    struct TestSigner {
        signing_key: lillux::crypto::SigningKey,
        fingerprint: String,
    }

    impl TestSigner {
        fn new(seed: u8) -> Self {
            let signing_key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
            let fingerprint = lillux::crypto::fingerprint(&signing_key.verifying_key());
            Self {
                signing_key,
                fingerprint,
            }
        }

        fn key(&self) -> lillux::crypto::VerifyingKey {
            self.signing_key.verifying_key()
        }
    }

    impl Signer for TestSigner {
        fn sign(&self, data: &[u8]) -> Vec<u8> {
            use lillux::crypto::Signer as Ed25519Signer;
            self.signing_key.sign(data).to_bytes().to_vec()
        }

        fn fingerprint(&self) -> &str {
            &self.fingerprint
        }

        fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
            self.signing_key.verifying_key()
        }
    }

    fn remote(name: &str, site_id: &str, signer: &TestSigner) -> RemoteConfig {
        let key = signer.key();
        RemoteConfig {
            name: name.to_owned(),
            url: format!("https://{name}.example.com"),
            principal_id: format!("fp:{}", lillux::crypto::fingerprint(&key)),
            signing_key: format!(
                "ed25519:{}",
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, key.as_bytes())
            ),
            site_id: site_id.to_owned(),
            vault_fingerprint: "sha256:test".to_owned(),
            ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
            project_bindings: HashMap::new(),
        }
    }

    fn offered_record(witness: &str, signer: &TestSigner) -> (String, Value) {
        let attestation = ryeos_state::Attestation::unsigned(
            witness.to_owned(),
            "accepted".to_owned(),
            ryeos_state::admission::LOCAL_ADMISSION_POLICY.to_owned(),
            "2026-10-22T00:00:00Z".to_owned(),
            None,
            serde_json::json!({}),
        )
        .sign(signer)
        .unwrap();
        let value = attestation.to_value();
        let hash = lillux::sha256_hex(lillux::canonical_json(&value).unwrap().as_bytes());
        let record = serde_json::json!({
            "attestation_hash": hash,
            "subject_hash": witness,
            "policy": ryeos_state::admission::LOCAL_ADMISSION_POLICY,
            "claim": "accepted",
            "issuer": format!("fp:{}", signer.fingerprint()),
            "issued_at": "2026-10-22T00:00:00Z",
            "expires_at": null,
            "head_ref_path": null,
            "indexed_at": "2026-10-22T00:00:01Z",
            "state": "accepted",
            "attestation": value,
        });
        (hash, record)
    }

    fn authority_for(signer: &TestSigner) -> PushAuthority {
        PushAuthority {
            owner_principal: format!("fp:{}", "11".repeat(32)),
            origin_key: signer.key(),
            expected_issuer: format!("fp:{}", signer.fingerprint()),
        }
    }

    fn request_for(witness: &str, hash: &str, record: Value) -> Request {
        Request {
            witness_hash: witness.to_owned(),
            origin_admission_hash: hash.to_owned(),
            maximum_bytes: 1024,
            origin_admission: record,
            objects: Vec::new(),
            staging_id: None,
        }
    }

    #[test]
    fn descriptor_names_the_offer_service() {
        assert_eq!(
            DESCRIPTOR.service_ref,
            "service:external-content/product-offer"
        );
        assert_eq!(DESCRIPTOR.endpoint, "external-content.product-offer");
        assert_eq!(
            DESCRIPTOR.required_caps,
            ["ryeos.execute.service.external-content/product-offer"]
        );
        assert!(matches!(
            DESCRIPTOR.availability,
            ServiceAvailability::DaemonOnly
        ));
    }

    #[test]
    fn request_shape_is_exact() {
        let valid = serde_json::json!({
            "witness_hash": "a".repeat(64),
            "origin_admission_hash": "b".repeat(64),
            "maximum_bytes": 1024,
            "origin_admission": {},
            "objects": [],
        });
        serde_json::from_value::<Request>(valid.clone())
            .unwrap()
            .validate()
            .unwrap();
        for field in [
            "witness_hash",
            "origin_admission_hash",
            "maximum_bytes",
            "origin_admission",
            "objects",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<Request>(missing).is_err(),
                "{field}"
            );
        }
        let mut unknown = valid.clone();
        unknown["signing_key"] = serde_json::json!("caller-selected-key");
        assert!(serde_json::from_value::<Request>(unknown).is_err());
        for (field, value) in [
            ("witness_hash", serde_json::json!("A".repeat(64))),
            ("origin_admission_hash", serde_json::json!("b".repeat(63))),
            ("maximum_bytes", serde_json::json!(0)),
            ("staging_id", serde_json::json!("")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<Request>(invalid)
                    .unwrap()
                    .validate()
                    .is_err(),
                "{field}"
            );
        }
        let mut too_many = valid;
        too_many["objects"] = Value::Array(vec![Value::Null; MAX_OFFERED_OBJECTS + 1]);
        assert!(
            serde_json::from_value::<Request>(too_many)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn stage_digest_binds_witness_admission_and_owner() {
        let base = stage_request_digest(&"a".repeat(64), &"b".repeat(64), "fp:one").unwrap();
        assert_eq!(
            base,
            stage_request_digest(&"a".repeat(64), &"b".repeat(64), "fp:one").unwrap()
        );
        for other in [
            stage_request_digest(&"c".repeat(64), &"b".repeat(64), "fp:one").unwrap(),
            stage_request_digest(&"a".repeat(64), &"c".repeat(64), "fp:one").unwrap(),
            stage_request_digest(&"a".repeat(64), &"b".repeat(64), "fp:two").unwrap(),
        ] {
            assert_ne!(base, other);
        }
    }

    #[test]
    fn origin_resolves_only_from_the_authenticated_site() {
        let origin = TestSigner::new(5);
        let other = TestSigner::new(6);
        let mut remotes = HashMap::new();
        remotes.insert(
            "origin".to_owned(),
            remote("origin", "site:origin", &origin),
        );
        remotes.insert("other".to_owned(), remote("other", "site:other", &other));
        let (key, issuer) = resolve_push_origin(&remotes, "site:origin").unwrap();
        assert_eq!(key.as_bytes(), origin.key().as_bytes());
        assert_eq!(issuer, format!("fp:{}", origin.fingerprint()));
        let error = resolve_push_origin(&remotes, "site:unknown")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("no configured remote for origin site"),
            "{error}"
        );
        let mut duplicate = remotes.clone();
        duplicate.insert("twin".to_owned(), remote("twin", "site:origin", &other));
        assert!(resolve_push_origin(&duplicate, "site:origin").is_err());
        let mut mismatched = remote("origin", "site:origin", &origin);
        mismatched.principal_id = format!("fp:{}", other.fingerprint());
        let mut broken = HashMap::new();
        broken.insert("origin".to_owned(), mismatched);
        assert!(resolve_push_origin(&broken, "site:origin").is_err());
    }

    #[test]
    fn offered_admission_verifies_like_the_pull_path() {
        let origin = TestSigner::new(5);
        let witness = "ab".repeat(32);
        let (hash, record) = offered_record(&witness, &origin);
        let authority = authority_for(&origin);
        let request = request_for(&witness, &hash, record.clone());
        verify_offered_admission(&request, &authority).unwrap();

        // A different pinned key refuses the origin's signature.
        let impostor = TestSigner::new(9);
        let wrong_key = authority_for(&impostor);
        assert!(verify_offered_admission(&request, &wrong_key).is_err());

        // A matching key with a different expected issuer is refused.
        let mut wrong_issuer = authority_for(&origin);
        wrong_issuer.expected_issuer = format!("fp:{}", impostor.fingerprint());
        let error = verify_offered_admission(&request, &wrong_issuer)
            .unwrap_err()
            .to_string();
        assert!(error.contains("issuer mismatch"), "{error}");

        // The record must be the one the request names.
        let named_elsewhere = request_for(&witness, &"cd".repeat(32), record.clone());
        let error = verify_offered_admission(&named_elsewhere, &authority)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not the named"), "{error}");

        // The record must be about the named witness.
        let other_witness = request_for(&"cd".repeat(32), &hash, record.clone());
        assert!(verify_offered_admission(&other_witness, &authority).is_err());

        // Tampering with the attestation body breaks its hash.
        let mut tampered = record;
        tampered["attestation"]["claim"] = serde_json::json!("rejected");
        let tampered = request_for(&witness, &hash, tampered);
        assert!(verify_offered_admission(&tampered, &authority).is_err());

        // A non-record value is refused as such.
        let malformed = request_for(&witness, &hash, serde_json::json!({"x": 1}));
        assert!(verify_offered_admission(&malformed, &authority).is_err());
    }

    fn context(
        class: Option<AuthorizedKeyPrincipalClass>,
        site_id: Option<&str>,
        verified: bool,
    ) -> HandlerContext {
        HandlerContext::new_with_authority(
            format!("fp:{}", "22".repeat(32)),
            Vec::new(),
            verified,
            class,
            site_id.map(str::to_owned),
        )
    }

    #[test]
    fn push_authentication_refuses_every_non_remote_operator_shape() {
        let root = tempfile::tempdir().unwrap();
        let (_owner, state) = ryeos_app::state::test_support::build(root.path()).unwrap();
        let refusals = [
            // Not signature-verified.
            context(
                Some(AuthorizedKeyPrincipalClass::RemoteOperator),
                Some("site:o"),
                false,
            ),
            // A local client that is not the configured operator.
            context(Some(AuthorizedKeyPrincipalClass::LocalClient), None, true),
            // A node key is never an operator.
            context(
                Some(AuthorizedKeyPrincipalClass::RemoteNode),
                Some("site:o"),
                true,
            ),
            // A remote operator without the source-node forwarding proof.
            context(
                Some(AuthorizedKeyPrincipalClass::RemoteOperator),
                None,
                true,
            ),
            // No class at all.
            context(None, Some("site:o"), true),
        ];
        for refused in &refusals {
            assert!(authenticate_push_origin(&state, refused).is_err());
        }
        // A node key is never an operator, whatever its message says.
        assert!(
            authenticate_push_origin(
                &state,
                &context(
                    Some(AuthorizedKeyPrincipalClass::RemoteNode),
                    Some("site:o"),
                    true,
                ),
            )
            .is_err()
        );
    }
}
