//! Typed authority boundary for native bundle producer operations.
//!
//! Build workers, qualification runners, publishers, and catalog sources are
//! independent authorities. The daemon composition root must install an
//! adapter that implements these exact operations; absence fails closed.

use std::{ffi::OsStr, io::Read as _};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_bundle_publication_contract::{
    BUNDLE_CATALOG_PUBLICATION_KIND, BUNDLE_CATALOG_PUBLICATION_SCHEMA,
    BUNDLE_CATALOG_SNAPSHOT_KIND, BUNDLE_CATALOG_SNAPSHOT_SCHEMA, BundleCatalogChannel,
    BundleCatalogPublication, BundleCatalogSnapshot, BundleGeneration, BundleSet, BundleSetEntry,
    PublisherMaterializationResult, SetCatalogChannel,
};
use ryeos_state::objects::Attestation;
use ryeos_state::objects::ExternalContentManifestObject;

const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_EVIDENCE_HASHES: usize = 64;

pub type ProducerFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send + 'a>>;

macro_rules! request {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
    };
}

request!(InputInspectRequest {
    project_path: String,
    bundle_name: String,
    source_snapshot_hash: String,
    target: Value,
    build_profile: String,
    predecessor_set_hash: String,
    catalog_namespace: String,
});
request!(GenerationBuildRequest {
    release_input: Value
});
request!(RequestTreeSigningRequest {
    accepted_product_result_hash: String,
    selected_product_identity: String,
    selected_product_witness: String,
    input_content_manifest_hash: String,
});
request!(GenerationCaptureRequest {
    materialization_result_hash: String,
    signed_tree_manifest_hash: String,
});
request!(GenerationQualifyRequest {
    release_input: Value,
    captured_tree_manifest_hash: String,
    manifest_item_hash: String,
});
request!(GenerationFinalizeRequest {
    generation: Value,
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
});
request!(RequestAuthorizationRequest {
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
    generation_hash: String,
    qualification_evidence_hashes: Vec<String>,
    expected_publisher_fingerprint: String,
});
request!(SetComposeRequest {
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
    predecessor_set_hash: String,
    replacement: Value,
    substrate_protocol: u32,
});
request!(CatalogRequestPublicationRequest {
    catalog_namespace: String,
    channel: String,
    bundle_name: String,
    generation_attestation_hash: String,
    bundle_set_hash: String,
    predecessor_publication_attestation_hash: Option<String>,
    expected_sequence: u64,
});
request!(SubmitRequest {
    project_path: String,
    catalog_remote: String,
    source_snapshot_hash: String,
    bundle_name: String,
    target: Value,
    build_profile: String,
    substrate_protocol: u64,
    predecessor_set_hash: String,
    predecessor_catalog_publication_attestation_hash: Option<String>,
    expected_catalog_sequence: u64,
    catalog_namespace: String,
    channel: String,
});
request!(StatusRequest {
    operation_id: String,
    include_events: Option<bool>,
    event_limit: Option<u32>,
});
request!(ExportRecoveryRequest {
    catalog_namespace: String,
    catalog_publication_attestation_hash: String,
    generation_publication_attestation_hashes: Vec<String>,
    set_publication_attestation_hashes: Vec<String>,
});
request!(RestoreGenesisRequest {
    catalog_namespace: String,
    recovery_manifest_hash: String,
    expected_catalog_head: Option<String>,
    bundle_publication_policy_section_digest: String,
    node_policy_generation_digest: String,
});

#[derive(Debug, Clone)]
pub enum BundleReleaseOperation {
    InputInspect(InputInspectRequest),
    GenerationBuild(GenerationBuildRequest),
    RequestTreeSigning(RequestTreeSigningRequest),
    GenerationCapture(GenerationCaptureRequest),
    GenerationQualify(GenerationQualifyRequest),
    GenerationFinalize(GenerationFinalizeRequest),
    RequestAuthorization(RequestAuthorizationRequest),
    SetCompose(SetComposeRequest),
    CatalogRequestPublication(CatalogRequestPublicationRequest),
    Submit(SubmitRequest),
    Status(StatusRequest),
    ExportRecovery(ExportRecoveryRequest),
    RestoreGenesis(RestoreGenesisRequest),
}

impl BundleReleaseOperation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::InputInspect(_) => "input_inspect",
            Self::GenerationBuild(_) => "generation_build",
            Self::RequestTreeSigning(_) => "request_tree_signing",
            Self::GenerationCapture(_) => "generation_capture",
            Self::GenerationQualify(_) => "generation_qualify",
            Self::GenerationFinalize(_) => "generation_finalize",
            Self::RequestAuthorization(_) => "request_authorization",
            Self::SetCompose(_) => "set_compose",
            Self::CatalogRequestPublication(_) => "catalog_request_publication",
            Self::Submit(_) => "submit",
            Self::Status(_) => "status",
            Self::ExportRecovery(_) => "export_recovery",
            Self::RestoreGenesis(_) => "restore_genesis",
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let encoded = serde_json::to_vec(&operation_value(self)?)?;
        anyhow::ensure!(
            encoded.len() <= MAX_REQUEST_BYTES,
            "bundle release request exceeds {MAX_REQUEST_BYTES} bytes"
        );
        match self {
            Self::InputInspect(r) => {
                canonical_absolute_path(&r.project_path)?;
                name(&r.bundle_name)?;
                hash(&r.source_snapshot_hash)?;
                hash(&r.predecessor_set_hash)?;
                name(&r.catalog_namespace)?;
                token(&r.build_profile, 32)?;
            }
            Self::GenerationBuild(r) => bounded_value(&r.release_input)?,
            Self::RequestTreeSigning(r) => {
                hash(&r.accepted_product_result_hash)?;
                token(&r.selected_product_identity, 128)?;
                hash(&r.selected_product_witness)?;
                hash(&r.input_content_manifest_hash)?;
            }
            Self::GenerationCapture(r) => {
                hash(&r.materialization_result_hash)?;
                hash(&r.signed_tree_manifest_hash)?;
            }
            Self::GenerationQualify(r) => {
                bounded_value(&r.release_input)?;
                hash(&r.captured_tree_manifest_hash)?;
                hash(&r.manifest_item_hash)?;
            }
            Self::GenerationFinalize(r) => {
                bounded_value(&r.generation)?;
                BundleGeneration::from_current_value(&r.generation)?;
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
            }
            Self::RequestAuthorization(r) => {
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
                hash(&r.generation_hash)?;
                hashes(&r.qualification_evidence_hashes)?;
                token(&r.expected_publisher_fingerprint, 256)?;
            }
            Self::SetCompose(r) => {
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
                hash(&r.predecessor_set_hash)?;
                bounded_value(&r.replacement)?;
                let replacement: BundleSetEntry = serde_json::from_value(r.replacement.clone())?;
                name(&replacement.bundle_name)?;
                hash(&replacement.generation_hash)?;
                hash(&replacement.publisher_attestation_hash)?;
                anyhow::ensure!(
                    r.substrate_protocol > 0,
                    "substrate protocol must be nonzero"
                );
            }
            Self::CatalogRequestPublication(r) => {
                token(&r.catalog_namespace, 256)?;
                token(&r.channel, 128)?;
                name(&r.bundle_name)?;
                hash(&r.generation_attestation_hash)?;
                hash(&r.bundle_set_hash)?;
                optional_hash(&r.predecessor_publication_attestation_hash)?;
            }
            Self::Submit(r) => {
                canonical_absolute_path(&r.project_path)?;
                name(&r.catalog_remote)?;
                hash(&r.source_snapshot_hash)?;
                name(&r.bundle_name)?;
                bounded_value(&r.target)?;
                token(&r.build_profile, 32)?;
                anyhow::ensure!(r.build_profile == "release", "unsupported build profile");
                anyhow::ensure!(
                    r.substrate_protocol > 0,
                    "substrate protocol must be nonzero"
                );
                hash(&r.predecessor_set_hash)?;
                optional_hash(&r.predecessor_catalog_publication_attestation_hash)?;
                token(&r.catalog_namespace, 256)?;
                token(&r.channel, 128)?;
            }
            Self::Status(r) => {
                token(&r.operation_id, 256)?;
                anyhow::ensure!(
                    r.event_limit.unwrap_or(100) <= 1000,
                    "event limit exceeds 1000"
                );
            }
            Self::ExportRecovery(r) => {
                token(&r.catalog_namespace, 256)?;
                hash(&r.catalog_publication_attestation_hash)?;
                hashes(&r.generation_publication_attestation_hashes)?;
                hashes(&r.set_publication_attestation_hashes)?;
            }
            Self::RestoreGenesis(r) => {
                token(&r.catalog_namespace, 256)?;
                hash(&r.recovery_manifest_hash)?;
                anyhow::ensure!(
                    r.expected_catalog_head.is_none(),
                    "restore genesis requires an absent expected catalog head"
                );
                hash(&r.bundle_publication_policy_section_digest)?;
                hash(&r.node_policy_generation_digest)?;
            }
        }
        Ok(())
    }
}

fn operation_value(operation: &BundleReleaseOperation) -> anyhow::Result<Value> {
    Ok(match operation {
        BundleReleaseOperation::InputInspect(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationBuild(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::RequestTreeSigning(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationCapture(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationQualify(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationFinalize(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::RequestAuthorization(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::SetCompose(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::CatalogRequestPublication(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::Submit(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::Status(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::ExportRecovery(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::RestoreGenesis(v) => serde_json::to_value(v)?,
    })
}
fn token(value: &str, max: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control),
        "invalid bounded token"
    );
    Ok(())
}
fn canonical_absolute_path(value: &str) -> anyhow::Result<()> {
    token(value, 4096)?;
    let path = std::path::Path::new(value);
    anyhow::ensure!(path.is_absolute(), "project path must be absolute");
    anyhow::ensure!(
        !path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
        "project path must not contain parent traversal"
    );
    Ok(())
}
fn name(value: &str) -> anyhow::Result<()> {
    token(value, 128)?;
    anyhow::ensure!(
        value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid canonical bundle name"
    );
    Ok(())
}
fn hash(value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid lowercase SHA-256 identity"
    );
    Ok(())
}
fn optional_hash(value: &Option<String>) -> anyhow::Result<()> {
    if let Some(v) = value {
        hash(v)?;
    }
    Ok(())
}
fn hashes(values: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        values.len() <= MAX_EVIDENCE_HASHES,
        "too many evidence hashes"
    );
    for v in values {
        hash(v)?;
    }
    anyhow::ensure!(
        values.windows(2).all(|p| p[0] < p[1]),
        "evidence hashes must be sorted and unique"
    );
    Ok(())
}
fn bounded_value(value: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(
        serde_json::to_vec(value)?.len() <= MAX_REQUEST_BYTES,
        "nested request value is too large"
    );
    Ok(())
}

pub trait BundleReleaseAuthorityAdapter: Send + Sync + 'static {
    fn execute<'a>(&'a self, operation: BundleReleaseOperation) -> ProducerFuture<'a>;

    fn begin_external(&self, _operation: &BundleReleaseOperation) -> anyhow::Result<String> {
        anyhow::bail!("bundle release adapter does not support externally dispatched operations")
    }

    fn complete_external(&self, _operation_id: &str, _result: &Value) -> anyhow::Result<()> {
        anyhow::bail!("bundle release adapter does not support externally dispatched operations")
    }

    fn fail_external(&self, _operation_id: &str, _error: &str) -> anyhow::Result<()> {
        anyhow::bail!("bundle release adapter does not support externally dispatched operations")
    }
}

pub trait BundleBuildAuthority: Send + Sync + 'static {
    fn inspect<'a>(&'a self, request: InputInspectRequest) -> ProducerFuture<'a>;
    fn build<'a>(&'a self, request: GenerationBuildRequest) -> ProducerFuture<'a>;
}

pub trait BundleQualificationAuthority: Send + Sync + 'static {
    fn qualify<'a>(&'a self, request: GenerationQualifyRequest) -> ProducerFuture<'a>;
}

/// Purpose-owned publisher authority. Keeping these operations together does
/// not grant a generic sign-any-hash primitive: each method accepts only its
/// closed current request contract.
pub trait BundlePublisherAuthority: Send + Sync + 'static {
    fn sign_tree<'a>(&'a self, request: RequestTreeSigningRequest) -> ProducerFuture<'a>;
    fn authorize_generation<'a>(
        &'a self,
        request: RequestAuthorizationRequest,
    ) -> ProducerFuture<'a>;
    fn authorize_catalog_successor<'a>(
        &'a self,
        request: CatalogRequestPublicationRequest,
    ) -> ProducerFuture<'a>;
}

/// Explicit authenticated client for publisher custody outside the RyeOS
/// daemon. The remote authority must share (or explicitly import into) this
/// node's CAS; responses only return typed CAS coordinates and never key
/// material. Configuration is injected by the process composition root and is
/// never discovered from `AppState` or the application root.
pub struct AuthenticatedPublisherClient {
    endpoint: url::Url,
    bearer: zeroize::Zeroizing<String>,
    expected_publisher_fingerprint: String,
    client: reqwest::Client,
}

impl AuthenticatedPublisherClient {
    pub fn new(
        endpoint: url::Url,
        bearer: String,
        expected_publisher_fingerprint: String,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            endpoint.scheme() == "https"
                || (endpoint.scheme() == "http"
                    && endpoint.host_str().is_some_and(|host| host == "localhost"
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback()))),
            "publisher endpoint must use HTTPS or loopback HTTP"
        );
        anyhow::ensure!(
            endpoint.username().is_empty() && endpoint.password().is_none(),
            "publisher endpoint must not contain credentials"
        );
        anyhow::ensure!(
            endpoint.query().is_none()
                && endpoint.fragment().is_none()
                && endpoint.path().ends_with('/'),
            "publisher endpoint must be a query-free base URL ending in `/`"
        );
        hash(&expected_publisher_fingerprint)?;
        token(&bearer, 4096)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        Ok(Self {
            endpoint,
            bearer: zeroize::Zeroizing::new(bearer),
            expected_publisher_fingerprint,
            client,
        })
    }

    async fn post<T: Serialize>(&self, path: &str, request: &T) -> anyhow::Result<Value> {
        let endpoint = self.endpoint.join(path)?;
        let response = self
            .client
            .post(endpoint)
            .bearer_auth(self.bearer.as_str())
            .json(request)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "publisher response exceeds the bounded response size"
        );
        anyhow::ensure!(
            status.is_success(),
            "publisher authority rejected the request with status {status}"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn verify_publisher_result(&self, value: &Value) -> anyhow::Result<()> {
        let fingerprint = value
            .get("publisher_fingerprint")
            .and_then(Value::as_str)
            .context("publisher response has no publisher fingerprint")?;
        anyhow::ensure!(
            fingerprint == self.expected_publisher_fingerprint,
            "publisher response came from an unexpected publisher"
        );
        Ok(())
    }
}

impl BundlePublisherAuthority for AuthenticatedPublisherClient {
    fn sign_tree<'a>(&'a self, request: RequestTreeSigningRequest) -> ProducerFuture<'a> {
        Box::pin(async move {
            let result = self.post("v1/bundle-tree/sign", &request).await?;
            self.verify_publisher_result(&result)?;
            let mut materialization = result.clone();
            let object = materialization
                .as_object_mut()
                .context("publisher tree-signing response is not an object")?;
            object.remove("materialization_result_hash");
            object.remove("result_schema");
            let materialization =
                PublisherMaterializationResult::from_current_value(&materialization)?;
            let returned_hash = result
                .get("materialization_result_hash")
                .and_then(Value::as_str)
                .context("publisher response has no materialization result hash")?;
            anyhow::ensure!(
                materialization.content_hash()? == returned_hash,
                "publisher response materialization hash disagrees with its object"
            );
            Ok(result)
        })
    }

    fn authorize_generation<'a>(
        &'a self,
        request: RequestAuthorizationRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            anyhow::ensure!(
                request.expected_publisher_fingerprint == self.expected_publisher_fingerprint,
                "generation authorization requested a different publisher"
            );
            let result = self
                .post("v1/bundle-generation/authorize", &request)
                .await?;
            self.verify_publisher_result(&result)?;
            Ok(result)
        })
    }

    fn authorize_catalog_successor<'a>(
        &'a self,
        request: CatalogRequestPublicationRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            let result = self
                .post("v1/bundle-catalog/authorize-successor", &request)
                .await?;
            self.verify_publisher_result(&result)?;
            Ok(result)
        })
    }
}

/// A purpose-owned publisher that can only sign a verified bundle-tree
/// mutation and authorize a generation which re-verifies against that exact
/// mutation. It is intentionally not installed by the ordinary daemon: a
/// release-authority composition root must inject publisher key custody.
pub struct LocalConstrainedPublisherAuthority {
    cas: Arc<lillux::CasStore>,
    tree_publisher: Arc<dyn super::publisher::ConstrainedBundleTreePublisher>,
    materialization_proof: Arc<dyn super::PublisherMaterializationProof>,
    release_evidence_proof: Arc<dyn super::BundleReleaseEvidenceProof>,
    signer: Arc<dyn ryeos_state::signer::Signer>,
}

impl LocalConstrainedPublisherAuthority {
    pub fn new(
        state_store: Arc<crate::state_store::StateStore>,
        tree_publisher: Arc<dyn super::publisher::ConstrainedBundleTreePublisher>,
        materialization_proof: Arc<dyn super::PublisherMaterializationProof>,
        release_evidence_proof: Arc<dyn super::BundleReleaseEvidenceProof>,
        signer: Arc<dyn ryeos_state::signer::Signer>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            lillux::crypto::fingerprint(&signer.verifying_key()) == signer.fingerprint(),
            "publisher signer fingerprint disagrees with its verifying key"
        );
        Ok(Self {
            cas: Arc::new(state_store.pinned_state_authority()?.cas_store()?),
            tree_publisher,
            materialization_proof,
            release_evidence_proof,
            signer,
        })
    }

    pub fn new_with_cas(
        cas: Arc<lillux::CasStore>,
        tree_publisher: Arc<dyn super::publisher::ConstrainedBundleTreePublisher>,
        materialization_proof: Arc<dyn super::PublisherMaterializationProof>,
        release_evidence_proof: Arc<dyn super::BundleReleaseEvidenceProof>,
        signer: Arc<dyn ryeos_state::signer::Signer>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            lillux::crypto::fingerprint(&signer.verifying_key()) == signer.fingerprint(),
            "publisher signer fingerprint disagrees with its verifying key"
        );
        Ok(Self {
            cas,
            tree_publisher,
            materialization_proof,
            release_evidence_proof,
            signer,
        })
    }

    fn cas(&self) -> anyhow::Result<&lillux::CasStore> {
        Ok(self.cas.as_ref())
    }
}

impl BundlePublisherAuthority for LocalConstrainedPublisherAuthority {
    fn sign_tree<'a>(&'a self, request: RequestTreeSigningRequest) -> ProducerFuture<'a> {
        Box::pin(async move {
            let cas = self.cas()?;
            let candidate = super::publisher::verify_publisher_candidate(
                super::publisher::PublisherCandidateCoordinate {
                    accepted_product_result_hash: request.accepted_product_result_hash,
                    selected_product_name: request.selected_product_identity,
                    selected_product_witness_hash: request.selected_product_witness,
                    input_content_manifest_hash: request.input_content_manifest_hash,
                },
                cas,
            )?;
            let materialization_result_hash = super::publisher::materialize_publisher_candidate(
                &candidate,
                self.tree_publisher.as_ref(),
                self.materialization_proof.as_ref(),
                cas,
            )?;
            let materialization = PublisherMaterializationResult::from_current_value(
                &super::read_exact(cas, &materialization_result_hash)?,
            )?;
            let mut result = materialization.to_value()?;
            let object = result
                .as_object_mut()
                .context("publisher materialization result is not an object")?;
            object.insert(
                "materialization_result_hash".to_owned(),
                Value::String(materialization_result_hash),
            );
            object.insert(
                "result_schema".to_owned(),
                Value::String("ryeos.bundle_tree_signing_result.v1".to_owned()),
            );
            Ok(result)
        })
    }

    fn authorize_generation<'a>(
        &'a self,
        request: RequestAuthorizationRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            anyhow::ensure!(
                request.expected_publisher_fingerprint == self.signer.fingerprint(),
                "generation authorization requested a different publisher"
            );
            let cas = self.cas()?;
            let verified = super::inspect_bundle_generation(
                &request.generation_hash,
                cas,
                self.materialization_proof.as_ref(),
                self.release_evidence_proof.as_ref(),
                &super::ReleasePolicyBinding {
                    catalog_namespace: request.catalog_namespace.clone(),
                    bundle_publication_policy_section_digest: request
                        .bundle_publication_policy_section_digest
                        .clone(),
                    trust_epoch: request.trust_epoch,
                },
            )?;
            anyhow::ensure!(
                verified.generation().qualification_evidence_hashes
                    == request.qualification_evidence_hashes,
                "generation authorization evidence differs from verified generation"
            );
            let attestation = super::attestation::authorize_generation_release(
                &verified,
                self.signer.as_ref(),
                &lillux::time::iso8601_now(),
            )?;
            let stored = cas.put_object(&attestation.to_value())?;
            Ok(serde_json::json!({
                "schema": "ryeos.bundle_generation_authorization_result.v1",
                "catalog_namespace": request.catalog_namespace,
                "generation_hash": request.generation_hash,
                "generation_attestation_hash": stored.hash,
                "publisher_fingerprint": self.signer.fingerprint(),
            }))
        })
    }

    fn authorize_catalog_successor<'a>(
        &'a self,
        request: CatalogRequestPublicationRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            use super::attestation::{
                BUNDLE_CATALOG_RELEASE_CLAIM, BUNDLE_PUBLICATION_POLICY, BUNDLE_SET_RELEASE_CLAIM,
            };

            let cas = self.cas()?;
            let set =
                BundleSet::from_current_value(&super::read_exact(cas, &request.bundle_set_hash)?)?;
            let replacement = set
                .entries
                .iter()
                .find(|entry| entry.bundle_name == request.bundle_name)
                .context("curated set does not contain the requested bundle")?;
            anyhow::ensure!(
                replacement.publisher_attestation_hash == request.generation_attestation_hash,
                "catalog generation coordinate disagrees with the curated set"
            );
            let generation_attestation = Attestation::from_value(&super::read_exact(
                cas,
                &request.generation_attestation_hash,
            )?)?;
            super::attestation::verify_release_attestation(
                &generation_attestation,
                &replacement.generation_hash,
                super::attestation::ReleaseSubjectKind::BundleGeneration,
                self.signer.fingerprint(),
                &self.signer.verifying_key(),
            )?;
            for entry in &set.entries {
                let attestation = Attestation::from_value(&super::read_exact(
                    cas,
                    &entry.publisher_attestation_hash,
                )?)?;
                super::attestation::verify_release_attestation(
                    &attestation,
                    &entry.generation_hash,
                    super::attestation::ReleaseSubjectKind::BundleGeneration,
                    self.signer.fingerprint(),
                    &self.signer.verifying_key(),
                )?;
            }

            let issued_at = lillux::time::iso8601_now();
            let set_attestation = Attestation::unsigned(
                request.bundle_set_hash.clone(),
                BUNDLE_SET_RELEASE_CLAIM.to_owned(),
                BUNDLE_PUBLICATION_POLICY.to_owned(),
                issued_at.clone(),
                None,
                serde_json::json!({"catalog_namespace": request.catalog_namespace}),
            )
            .sign(self.signer.as_ref())?;
            let set_attestation_hash = cas
                .put_object(&serde_json::to_value(&set_attestation)?)?
                .hash;

            let mut snapshot = match request.predecessor_publication_attestation_hash.as_deref() {
                Some(predecessor_hash) => {
                    let predecessor_attestation =
                        Attestation::from_value(&super::read_exact(cas, predecessor_hash)?)?;
                    super::attestation::verify_release_attestation(
                        &predecessor_attestation,
                        &predecessor_attestation.subject_hash,
                        super::attestation::ReleaseSubjectKind::BundleCatalogPublication,
                        self.signer.fingerprint(),
                        &self.signer.verifying_key(),
                    )?;
                    let predecessor = BundleCatalogPublication::from_current_value(
                        &super::read_exact(cas, &predecessor_attestation.subject_hash)?,
                    )?;
                    anyhow::ensure!(
                        predecessor.catalog_namespace == request.catalog_namespace,
                        "catalog predecessor belongs to another namespace"
                    );
                    anyhow::ensure!(
                        predecessor.sequence.checked_add(1) == Some(request.expected_sequence),
                        "catalog successor sequence is not predecessor plus one"
                    );
                    BundleCatalogSnapshot::from_current_value(&super::read_exact(
                        cas,
                        &predecessor.snapshot_hash,
                    )?)?
                }
                None => {
                    anyhow::ensure!(
                        request.expected_sequence == 0,
                        "catalog genesis requires sequence zero"
                    );
                    BundleCatalogSnapshot {
                        schema: BUNDLE_CATALOG_SNAPSHOT_SCHEMA.to_owned(),
                        kind: BUNDLE_CATALOG_SNAPSHOT_KIND.to_owned(),
                        publisher: format!("fp:{}", self.signer.fingerprint()),
                        bundle_channels: Vec::new(),
                        set_channels: Vec::new(),
                    }
                }
            };
            anyhow::ensure!(
                snapshot.publisher == format!("fp:{}", self.signer.fingerprint()),
                "catalog predecessor belongs to another publisher"
            );
            upsert_bundle_channel(
                &mut snapshot.bundle_channels,
                BundleCatalogChannel {
                    bundle_name: request.bundle_name,
                    channel: request.channel.clone(),
                    generation_attestation_hash: request.generation_attestation_hash,
                },
            );
            upsert_set_channel(
                &mut snapshot.set_channels,
                SetCatalogChannel {
                    set_name: set.set_name,
                    channel: request.channel,
                    set_attestation_hash: set_attestation_hash.clone(),
                },
            );
            let snapshot_hash = cas.put_object(&snapshot.to_value()?)?.hash;
            let publication = BundleCatalogPublication {
                schema: BUNDLE_CATALOG_PUBLICATION_SCHEMA.to_owned(),
                kind: BUNDLE_CATALOG_PUBLICATION_KIND.to_owned(),
                catalog_namespace: request.catalog_namespace.clone(),
                snapshot_hash,
                previous_publication_attestation_hash: request
                    .predecessor_publication_attestation_hash,
                sequence: request.expected_sequence,
            };
            let publication_hash = cas.put_object(&publication.to_value()?)?.hash;
            let publication_attestation = Attestation::unsigned(
                publication_hash,
                BUNDLE_CATALOG_RELEASE_CLAIM.to_owned(),
                BUNDLE_PUBLICATION_POLICY.to_owned(),
                issued_at,
                None,
                serde_json::json!({"catalog_namespace": request.catalog_namespace}),
            )
            .sign(self.signer.as_ref())?;
            let publication_attestation_hash = cas
                .put_object(&serde_json::to_value(&publication_attestation)?)?
                .hash;
            Ok(serde_json::json!({
                "schema": "ryeos.bundle_catalog_authorization_result.v1",
                "catalog_namespace": request.catalog_namespace,
                "sequence": request.expected_sequence,
                "set_attestation_hash": set_attestation_hash,
                "publication_attestation_hash": publication_attestation_hash,
                "publisher_fingerprint": self.signer.fingerprint(),
            }))
        })
    }
}

fn upsert_bundle_channel(
    channels: &mut Vec<BundleCatalogChannel>,
    replacement: BundleCatalogChannel,
) {
    let key = (&replacement.bundle_name, &replacement.channel);
    match channels.binary_search_by(|entry| (&entry.bundle_name, &entry.channel).cmp(&key)) {
        Ok(index) => channels[index] = replacement,
        Err(index) => channels.insert(index, replacement),
    }
}

fn upsert_set_channel(channels: &mut Vec<SetCatalogChannel>, replacement: SetCatalogChannel) {
    let key = (&replacement.set_name, &replacement.channel);
    match channels.binary_search_by(|entry| (&entry.set_name, &entry.channel).cmp(&key)) {
        Ok(index) => channels[index] = replacement,
        Err(index) => channels.insert(index, replacement),
    }
}

/// Non-signing CAS operations. Implementations must require contextual
/// materialization/release/admission proofs; structural decoding alone is not
/// sufficient to finalize or compose a release.
pub trait BundleCasCompositionAuthority: Send + Sync + 'static {
    fn capture<'a>(&'a self, request: GenerationCaptureRequest) -> ProducerFuture<'a>;
    fn finalize<'a>(&'a self, request: GenerationFinalizeRequest) -> ProducerFuture<'a>;
    fn compose_set<'a>(&'a self, request: SetComposeRequest) -> ProducerFuture<'a>;
}

pub trait BundleCatalogRecoveryAuthority: Send + Sync + 'static {
    fn export<'a>(&'a self, request: ExportRecoveryRequest) -> ProducerFuture<'a>;
    fn restore_genesis<'a>(&'a self, request: RestoreGenesisRequest) -> ProducerFuture<'a>;
}

#[derive(Default)]
pub struct BundleReleaseAuthorityRouter {
    pub build: Option<Arc<dyn BundleBuildAuthority>>,
    pub qualification: Option<Arc<dyn BundleQualificationAuthority>>,
    pub publisher: Option<Arc<dyn BundlePublisherAuthority>>,
    pub cas_composition: Option<Arc<dyn BundleCasCompositionAuthority>>,
    pub recovery: Option<Arc<dyn BundleCatalogRecoveryAuthority>>,
}

/// Locally safe CAS operations available without publisher keys. Capture is a
/// verification boundary over an already retained publisher result; it never
/// signs content or manufactures qualification. Finalization and set
/// composition stay unavailable until their contextual proof authorities are
/// explicitly installed.
pub struct VerifiedLocalCasCompositionAuthority {
    state_store: Arc<crate::state_store::StateStore>,
}

/// Fully admitted local CAS authority. The proofs and publication policy are
/// injected by the composition root; this type never discovers signing keys
/// and never treats object presence as evidence.
pub struct AdmittedLocalCasCompositionAuthority {
    state_store: Arc<crate::state_store::StateStore>,
    materialization_proof: Arc<dyn super::PublisherMaterializationProof + Send + Sync>,
    release_evidence_proof: Arc<dyn super::BundleReleaseEvidenceProof + Send + Sync>,
    consumer_policy: Arc<dyn super::consumer::ConsumerPublicationPolicy + Send + Sync>,
    expected_publisher: String,
}

impl AdmittedLocalCasCompositionAuthority {
    pub fn new(
        state_store: Arc<crate::state_store::StateStore>,
        materialization_proof: Arc<dyn super::PublisherMaterializationProof + Send + Sync>,
        release_evidence_proof: Arc<dyn super::BundleReleaseEvidenceProof + Send + Sync>,
        consumer_policy: Arc<dyn super::consumer::ConsumerPublicationPolicy + Send + Sync>,
        expected_publisher: String,
    ) -> anyhow::Result<Self> {
        token(&expected_publisher, 256)?;
        Ok(Self {
            state_store,
            materialization_proof,
            release_evidence_proof,
            consumer_policy,
            expected_publisher,
        })
    }

    fn cas(&self) -> anyhow::Result<lillux::CasStore> {
        self.state_store.pinned_state_authority()?.cas_store()
    }

    fn finalize_exact(&self, request: GenerationFinalizeRequest) -> anyhow::Result<Value> {
        let generation = BundleGeneration::from_current_value(&request.generation)?;
        let cas = self.cas()?;
        let generation_hash = super::finalize_bundle_generation(
            generation,
            &cas,
            self.materialization_proof.as_ref(),
            self.release_evidence_proof.as_ref(),
            &super::ReleasePolicyBinding {
                catalog_namespace: request.catalog_namespace,
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest,
                trust_epoch: request.trust_epoch,
            },
        )?;
        Ok(serde_json::json!({
            "schema": "ryeos.bundle_generation_finalize_result.v1",
            "generation_hash": generation_hash,
        }))
    }

    fn compose_exact_set(&self, request: SetComposeRequest) -> anyhow::Result<Value> {
        let cas = self.cas()?;
        let mut predecessor = BundleSet::from_current_value(&super::read_exact(
            &cas,
            &request.predecessor_set_hash,
        )?)?;
        anyhow::ensure!(
            predecessor.substrate_protocol == request.substrate_protocol,
            "set composition substrate protocol disagrees with predecessor"
        );
        let replacement: BundleSetEntry = serde_json::from_value(request.replacement)?;
        match predecessor
            .entries
            .binary_search_by(|entry| entry.bundle_name.cmp(&replacement.bundle_name))
        {
            Ok(index) => predecessor.entries[index] = replacement,
            Err(index) => predecessor.entries.insert(index, replacement),
        }
        predecessor.validate()?;
        let set_hash = predecessor.content_hash()?;
        super::consumer::verify_prospective_set(
            set_hash.clone(),
            String::new(),
            predecessor.clone(),
            Some(&self.expected_publisher),
            &cas,
            self.consumer_policy.as_ref(),
            self.materialization_proof.as_ref(),
            self.release_evidence_proof.as_ref(),
            &super::ReleasePolicyBinding {
                catalog_namespace: request.catalog_namespace,
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest,
                trust_epoch: request.trust_epoch,
            },
        )?;
        let stored = cas.put_object(&predecessor.to_value()?)?;
        anyhow::ensure!(
            stored.hash == set_hash,
            "stored bundle set identity changed"
        );
        Ok(serde_json::json!({
            "schema": "ryeos.bundle_set_compose_result.v1",
            "predecessor_set_hash": request.predecessor_set_hash,
            "bundle_set_hash": set_hash,
        }))
    }
}

impl BundleCasCompositionAuthority for AdmittedLocalCasCompositionAuthority {
    fn capture<'a>(&'a self, request: GenerationCaptureRequest) -> ProducerFuture<'a> {
        Box::pin(async move {
            VerifiedLocalCasCompositionAuthority::new(Arc::clone(&self.state_store))
                .verify_capture(request)
        })
    }

    fn finalize<'a>(&'a self, request: GenerationFinalizeRequest) -> ProducerFuture<'a> {
        Box::pin(async move { self.finalize_exact(request) })
    }

    fn compose_set<'a>(&'a self, request: SetComposeRequest) -> ProducerFuture<'a> {
        Box::pin(async move { self.compose_exact_set(request) })
    }
}

impl VerifiedLocalCasCompositionAuthority {
    pub fn new(state_store: Arc<crate::state_store::StateStore>) -> Self {
        Self { state_store }
    }

    fn verify_capture(&self, request: GenerationCaptureRequest) -> anyhow::Result<Value> {
        let authority = self.state_store.pinned_state_authority()?;
        let cas = authority.cas_store()?;
        let materialization = PublisherMaterializationResult::from_current_value(
            &super::read_exact(&cas, &request.materialization_result_hash)?,
        )?;
        anyhow::ensure!(
            materialization.output_content_manifest_hash == request.signed_tree_manifest_hash,
            "capture request and publisher materialization disagree on signed tree"
        );
        let manifest = ExternalContentManifestObject::from_value(&super::read_exact(
            &cas,
            &request.signed_tree_manifest_hash,
        )?)?;
        super::tree::validate_native_bundle_tree(&manifest)?;
        Ok(serde_json::json!({
            "schema": "ryeos.bundle_generation_capture.v1",
            "materialization_result_hash": request.materialization_result_hash,
            "content_manifest_hash": request.signed_tree_manifest_hash,
            "manifest_item_hash": materialization.output_manifest_item_hash,
            "publisher_fingerprint": materialization.publisher_fingerprint,
        }))
    }
}

impl BundleCasCompositionAuthority for VerifiedLocalCasCompositionAuthority {
    fn capture<'a>(&'a self, request: GenerationCaptureRequest) -> ProducerFuture<'a> {
        Box::pin(async move { self.verify_capture(request) })
    }

    fn finalize<'a>(&'a self, _request: GenerationFinalizeRequest) -> ProducerFuture<'a> {
        Box::pin(async move {
            anyhow::bail!(
                "generation finalization requires explicitly composed materialization and release-evidence proof authorities"
            )
        })
    }

    fn compose_set<'a>(&'a self, _request: SetComposeRequest) -> ProducerFuture<'a> {
        Box::pin(async move {
            anyhow::bail!(
                "bundle-set composition requires an explicitly composed complete prospective-admission authority"
            )
        })
    }
}

impl BundleReleaseAuthorityRouter {
    /// Install the single admitted owner for the two non-signing producer
    /// stages. Publisher custody remains an independent composition choice.
    pub fn with_native_build_and_qualification(
        mut self,
        authority: Arc<super::admitted_build::AdmittedNativeBundleAuthorities>,
    ) -> Self {
        self.build = Some(authority.clone());
        self.qualification = Some(authority);
        self
    }

    fn unavailable(operation: &'static str, authority: &'static str) -> ProducerFuture<'static> {
        Box::pin(async move {
            anyhow::bail!(
                "bundle release operation {operation} requires the explicitly composed {authority} authority"
            )
        })
    }

    fn execute<'a>(&'a self, operation: BundleReleaseOperation) -> ProducerFuture<'a> {
        match operation {
            BundleReleaseOperation::InputInspect(request) => match &self.build {
                Some(authority) => authority.inspect(request),
                None => Self::unavailable("input_inspect", "build/inspection"),
            },
            BundleReleaseOperation::GenerationBuild(request) => match &self.build {
                Some(authority) => authority.build(request),
                None => Self::unavailable("generation_build", "build/inspection"),
            },
            BundleReleaseOperation::RequestTreeSigning(request) => match &self.publisher {
                Some(authority) => authority.sign_tree(request),
                None => Self::unavailable("request_tree_signing", "constrained publisher"),
            },
            BundleReleaseOperation::GenerationCapture(request) => match &self.cas_composition {
                Some(authority) => authority.capture(request),
                None => Self::unavailable("generation_capture", "CAS composition"),
            },
            BundleReleaseOperation::GenerationQualify(request) => match &self.qualification {
                Some(authority) => authority.qualify(request),
                None => Self::unavailable("generation_qualify", "qualification"),
            },
            BundleReleaseOperation::GenerationFinalize(request) => match &self.cas_composition {
                Some(authority) => authority.finalize(request),
                None => Self::unavailable("generation_finalize", "CAS composition"),
            },
            BundleReleaseOperation::RequestAuthorization(request) => match &self.publisher {
                Some(authority) => authority.authorize_generation(request),
                None => Self::unavailable("request_authorization", "constrained publisher"),
            },
            BundleReleaseOperation::SetCompose(request) => match &self.cas_composition {
                Some(authority) => authority.compose_set(request),
                None => Self::unavailable("set_compose", "CAS composition"),
            },
            BundleReleaseOperation::CatalogRequestPublication(request) => match &self.publisher {
                Some(authority) => authority.authorize_catalog_successor(request),
                None => Self::unavailable("catalog_request_publication", "constrained publisher"),
            },
            BundleReleaseOperation::Submit(_) => Self::unavailable(
                "submit",
                "per-call authenticated release Graph dispatch endpoint",
            ),
            BundleReleaseOperation::ExportRecovery(request) => match &self.recovery {
                Some(authority) => authority.export(request),
                None => Self::unavailable("export_recovery", "catalog recovery"),
            },
            BundleReleaseOperation::RestoreGenesis(request) => match &self.recovery {
                Some(authority) => authority.restore_genesis(request),
                None => Self::unavailable("restore_genesis", "catalog recovery"),
            },
            BundleReleaseOperation::Status(_) => {
                Self::unavailable("status", "persistent lifecycle")
            }
        }
    }
}

/// Durable local operation owner. Status is always local. Privileged work is
/// forwarded only to an explicitly supplied authority adapter and every
/// outcome is persisted before it is returned.
pub struct PersistentBundleReleaseAdapter {
    operation_root: PathBuf,
    policy: crate::node_policy::sections::bundle_publication::BundlePublicationPolicy,
    authorities: BundleReleaseAuthorityRouter,
}

impl PersistentBundleReleaseAdapter {
    pub fn new(
        app_root: PathBuf,
        policy: crate::node_policy::sections::bundle_publication::BundlePublicationPolicy,
        authorities: BundleReleaseAuthorityRouter,
    ) -> Self {
        Self {
            operation_root: app_root.join(".ai/state/bundle-release/operations"),
            policy,
            authorities,
        }
    }

    fn operation_path(&self, id: &str) -> anyhow::Result<PathBuf> {
        token(id, 256)?;
        anyhow::ensure!(
            id.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
            "operation id is not a canonical path-safe token"
        );
        Ok(self.operation_root.join(format!("{id}.json")))
    }

    fn write_record(&self, id: &str, value: &Value) -> anyhow::Result<()> {
        let directory = lillux::PinnedDirectory::open_or_create(&self.operation_root)?;
        let name = format!("{id}.json");
        let incumbent = directory.open_regular(OsStr::new(&name), false)?;
        let bytes = lillux::canonical_json(value)?.into_bytes();
        directory.atomic_write_if_same(OsStr::new(&name), incumbent.as_ref(), &bytes, 0o600)?;
        directory.ensure_path_binding()?;
        Ok(())
    }

    fn status(&self, request: &StatusRequest) -> anyhow::Result<Value> {
        let directory =
            lillux::PinnedDirectory::open(&self.operation_root)?.with_context(|| {
                format!(
                    "bundle release operation {} is absent",
                    request.operation_id
                )
            })?;
        let name = format!("{}.json", request.operation_id);
        let mut file = directory
            .open_regular(OsStr::new(&name), false)?
            .with_context(|| {
                format!(
                    "bundle release operation {} is absent",
                    request.operation_id
                )
            })?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take((MAX_REQUEST_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        directory.ensure_path_binding()?;
        anyhow::ensure!(
            bytes.len() <= MAX_REQUEST_BYTES,
            "bundle release operation record exceeds bound"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn check_policy(&self, operation: &BundleReleaseOperation) -> anyhow::Result<()> {
        let namespace = match operation {
            BundleReleaseOperation::RequestAuthorization(r) => Some(r.catalog_namespace.as_str()),
            BundleReleaseOperation::CatalogRequestPublication(r) => {
                Some(r.catalog_namespace.as_str())
            }
            BundleReleaseOperation::Submit(r) => Some(r.catalog_namespace.as_str()),
            BundleReleaseOperation::ExportRecovery(r) => Some(r.catalog_namespace.as_str()),
            BundleReleaseOperation::RestoreGenesis(r) => Some(r.catalog_namespace.as_str()),
            _ => None,
        };
        if let Some(namespace) = namespace {
            let catalog = self.policy.require_catalog(namespace)?;
            anyhow::ensure!(
                !catalog.frozen,
                "bundle publication catalog is frozen by node policy"
            );
        }
        Ok(())
    }
}

impl BundleReleaseAuthorityAdapter for PersistentBundleReleaseAdapter {
    fn execute<'a>(&'a self, operation: BundleReleaseOperation) -> ProducerFuture<'a> {
        Box::pin(async move {
            operation.validate()?;
            if let BundleReleaseOperation::Status(request) = &operation {
                return self.status(request);
            }
            self.check_policy(&operation)?;
            let operation_id = uuid::Uuid::new_v4().to_string();
            self.write_record(&operation_id, &serde_json::json!({"schema":"ryeos.bundle_release_operation.v1","operation_id":operation_id,"operation":operation.name(),"state":"submitted"}))?;
            let outcome = self.authorities.execute(operation).await;
            match outcome {
                Ok(mut result) => {
                    let object = result.as_object_mut().context(
                        "bundle release authority returned a non-object result",
                    )?;
                    anyhow::ensure!(
                        !object.contains_key("operation_id"),
                        "bundle release authority result attempted to own operation_id"
                    );
                    object.insert(
                        "operation_id".to_owned(),
                        Value::String(operation_id.clone()),
                    );
                    self.write_record(&operation_id, &serde_json::json!({"schema":"ryeos.bundle_release_operation.v1","operation_id":operation_id,"state":"completed","result":result}))?;
                    Ok(result)
                }
                Err(error) => {
                    self.write_record(&operation_id, &serde_json::json!({"schema":"ryeos.bundle_release_operation.v1","operation_id":operation_id,"state":"failed","error":format!("{error:#}")}))?;
                    Err(error)
                }
            }
            .map_err(|error| {
                anyhow::anyhow!(
                    "bundle release operation {operation_id} failed: {error:#}"
                )
            })
        })
    }

    fn begin_external(&self, operation: &BundleReleaseOperation) -> anyhow::Result<String> {
        operation.validate()?;
        self.check_policy(operation)?;
        anyhow::ensure!(
            matches!(operation, BundleReleaseOperation::Submit(_)),
            "only release Graph submission is externally dispatched"
        );
        let operation_id = uuid::Uuid::new_v4().to_string();
        self.write_record(
            &operation_id,
            &serde_json::json!({
                "schema":"ryeos.bundle_release_operation.v1",
                "operation_id":operation_id,
                "operation":operation.name(),
                "state":"submitted"
            }),
        )?;
        Ok(operation_id)
    }

    fn complete_external(&self, operation_id: &str, result: &Value) -> anyhow::Result<()> {
        self.write_record(
            operation_id,
            &serde_json::json!({
                "schema":"ryeos.bundle_release_operation.v1",
                "operation_id":operation_id,
                "state":"completed",
                "result":result
            }),
        )
    }

    fn fail_external(&self, operation_id: &str, error: &str) -> anyhow::Result<()> {
        self.write_record(
            operation_id,
            &serde_json::json!({
                "schema":"ryeos.bundle_release_operation.v1",
                "operation_id":operation_id,
                "state":"failed",
                "error":error
            }),
        )
    }
}

/// Extension-state wrapper. It is deliberately distinct from the adapter so
/// the type-erased extension bag never needs to downcast an unsized trait.
pub struct BundleReleaseAuthorities {
    adapter: Arc<dyn BundleReleaseAuthorityAdapter>,
}

impl BundleReleaseAuthorities {
    pub fn new(adapter: Arc<dyn BundleReleaseAuthorityAdapter>) -> Self {
        Self { adapter }
    }

    pub async fn execute(&self, operation: BundleReleaseOperation) -> anyhow::Result<Value> {
        operation.validate()?;
        self.adapter.execute(operation).await
    }

    pub fn begin_external(&self, operation: &BundleReleaseOperation) -> anyhow::Result<String> {
        self.adapter.begin_external(operation)
    }

    pub fn complete_external(&self, operation_id: &str, result: &Value) -> anyhow::Result<()> {
        self.adapter.complete_external(operation_id, result)
    }

    pub fn fail_external(&self, operation_id: &str, error: &str) -> anyhow::Result<()> {
        self.adapter.fail_external(operation_id, error)
    }
}
