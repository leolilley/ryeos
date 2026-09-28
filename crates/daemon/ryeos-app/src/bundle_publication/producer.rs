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
request!(CoreSeedInspectRequest {
    project_path: String,
    bundle_name: String,
    source_snapshot_hash: String,
    target: Value,
    build_profile: String,
    catalog_namespace: String,
});
request!(GenerationBuildRequest {
    release_input: Value,
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
});
request!(RequestTreeSigningRequest {
    accepted_product_result_hash: String,
    selected_product_identity: String,
    selected_product_witness: String,
    input_content_manifest_hash: String,
});
request!(GenerationCaptureRequest {
    release_input: Value,
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
    selected_product_witness: String,
    build_recipe_signed_config: String,
    build_recipe_raw_digest: String,
    materialization_result_hash: String,
    signed_tree_manifest_hash: String,
});
request!(GenerationQualifyRequest {
    release_input: Value,
    catalog_namespace: String,
    captured_tree_manifest_hash: String,
    manifest_item_hash: String,
    signed_product_witness: String,
    build_recipe_signed_config: String,
    capture_recipe_signed_config: String,
    capture_recipe_raw_digest: String,
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
request!(SubstrateReleaseAuthorizationRequest {
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
    substrate_release_hash: String,
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
request!(SubstrateReleaseFinalizeRequest {
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
    substrate_image_digest: String,
    substrate_protocol: u32,
    target: ryeos_bundle_publication_contract::BundleTarget,
    substrate_build_accepted_result_hash: String,
    substrate_build_receipt_hash: String,
    selected_substrate_product_identity: String,
    selected_substrate_product_witness: String,
    qualification_evidence_hashes: Vec<String>,
    core_generation_hash: String,
    core_generation_attestation_hash: String,
});

impl SubstrateReleaseFinalizeRequest {
    /// Derive the immutable release descriptor from closed receipt-stage
    /// coordinates. Callers cannot submit arbitrary descriptor fields, and
    /// the receipt itself is already a captured member of the authenticated
    /// substrate product, avoiding a release-hash/build-receipt cycle.
    pub fn release(&self) -> anyhow::Result<ryeos_bundle_publication_contract::SubstrateRelease> {
        let release = ryeos_bundle_publication_contract::SubstrateRelease {
            schema: ryeos_bundle_publication_contract::SUBSTRATE_RELEASE_SCHEMA.to_owned(),
            kind: ryeos_bundle_publication_contract::SUBSTRATE_RELEASE_KIND.to_owned(),
            catalog_namespace: self.catalog_namespace.clone(),
            bundle_publication_policy_section_digest: self
                .bundle_publication_policy_section_digest
                .clone(),
            trust_epoch: self.trust_epoch,
            substrate_image_digest: self.substrate_image_digest.clone(),
            substrate_protocol: self.substrate_protocol,
            target: self.target.clone(),
            substrate_build_accepted_result_hash: self.substrate_build_accepted_result_hash.clone(),
            substrate_build_receipt_hash: self.substrate_build_receipt_hash.clone(),
            selected_substrate_product_identity: self.selected_substrate_product_identity.clone(),
            selected_substrate_product_witness: self.selected_substrate_product_witness.clone(),
            qualification_evidence_hashes: self.qualification_evidence_hashes.clone(),
            core_generation_hash: self.core_generation_hash.clone(),
            core_generation_attestation_hash: self.core_generation_attestation_hash.clone(),
        };
        release.validate()?;
        Ok(release)
    }
}
request!(GenesisSetComposeRequest {
    bundle_set: Value,
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
});
request!(CatalogRequestPublicationRequest {
    catalog_namespace: String,
    bundle_publication_policy_section_digest: String,
    trust_epoch: u64,
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
    CoreSeedBuild(super::core_seed::CoreSeedBuildRequest),
    AuthorizeCoreSeedRecipe(super::core_seed::CoreSeedRecipeRequest),
    InputInspect(InputInspectRequest),
    AuthorizeBuildRecipe(super::recipe::AuthorizeBuildRecipeRequest),
    AuthorizeCaptureRecipe(super::recipe::AuthorizeCaptureRecipeRequest),
    AuthorizeSubstrateBuildRecipe(super::recipe::AuthorizeSubstrateBuildRecipeRequest),
    GenerationBuild(GenerationBuildRequest),
    RequestTreeSigning(RequestTreeSigningRequest),
    GenerationCapture(GenerationCaptureRequest),
    GenerationQualify(GenerationQualifyRequest),
    GenerationFinalize(GenerationFinalizeRequest),
    RequestAuthorization(RequestAuthorizationRequest),
    SubstrateReleaseAuthorization(SubstrateReleaseAuthorizationRequest),
    SetCompose(SetComposeRequest),
    SubstrateReleaseFinalize(SubstrateReleaseFinalizeRequest),
    GenesisSetCompose(GenesisSetComposeRequest),
    CatalogRequestPublication(CatalogRequestPublicationRequest),
    Submit(SubmitRequest),
    Status(StatusRequest),
    ExportRecovery(ExportRecoveryRequest),
    RestoreGenesis(RestoreGenesisRequest),
}

impl BundleReleaseOperation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::CoreSeedBuild(_) => "core_seed_build",
            Self::AuthorizeCoreSeedRecipe(_) => "authorize_core_seed_recipe",
            Self::InputInspect(_) => "input_inspect",
            Self::AuthorizeBuildRecipe(_) => "authorize_build_recipe",
            Self::AuthorizeCaptureRecipe(_) => "authorize_capture_recipe",
            Self::AuthorizeSubstrateBuildRecipe(_) => "authorize_substrate_build_recipe",
            Self::GenerationBuild(_) => "generation_build",
            Self::RequestTreeSigning(_) => "request_tree_signing",
            Self::GenerationCapture(_) => "generation_capture",
            Self::GenerationQualify(_) => "generation_qualify",
            Self::GenerationFinalize(_) => "generation_finalize",
            Self::RequestAuthorization(_) => "request_authorization",
            Self::SubstrateReleaseAuthorization(_) => "substrate_release_authorization",
            Self::SetCompose(_) => "set_compose",
            Self::SubstrateReleaseFinalize(_) => "substrate_release_finalize",
            Self::GenesisSetCompose(_) => "genesis_set_compose",
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
            Self::CoreSeedBuild(request) => request.validate()?,
            Self::AuthorizeCoreSeedRecipe(request) => request.validate()?,
            Self::InputInspect(r) => {
                canonical_absolute_path(&r.project_path)?;
                name(&r.bundle_name)?;
                hash(&r.source_snapshot_hash)?;
                hash(&r.predecessor_set_hash)?;
                name(&r.catalog_namespace)?;
                token(&r.build_profile, 32)?;
            }
            Self::AuthorizeBuildRecipe(r) => r.validate()?,
            Self::AuthorizeCaptureRecipe(r) => r.validate()?,
            Self::AuthorizeSubstrateBuildRecipe(r) => r.validate()?,
            Self::GenerationBuild(r) => {
                bounded_value(&r.release_input)?;
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
            }
            Self::RequestTreeSigning(r) => {
                hash(&r.accepted_product_result_hash)?;
                token(&r.selected_product_identity, 128)?;
                hash(&r.selected_product_witness)?;
                hash(&r.input_content_manifest_hash)?;
            }
            Self::GenerationCapture(r) => {
                bounded_value(&r.release_input)?;
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
                hash(&r.selected_product_witness)?;
                anyhow::ensure!(
                    !r.build_recipe_signed_config.is_empty()
                        && r.build_recipe_signed_config.len() <= MAX_REQUEST_BYTES,
                    "build recipe signed Config is invalid"
                );
                hash(&r.build_recipe_raw_digest)?;
                hash(&r.materialization_result_hash)?;
                hash(&r.signed_tree_manifest_hash)?;
            }
            Self::GenerationQualify(r) => {
                bounded_value(&r.release_input)?;
                hash(&r.captured_tree_manifest_hash)?;
                hash(&r.manifest_item_hash)?;
                hash(&r.signed_product_witness)?;
                hash(&r.capture_recipe_raw_digest)?;
                anyhow::ensure!(
                    !r.build_recipe_signed_config.is_empty()
                        && !r.capture_recipe_signed_config.is_empty(),
                    "qualification recipe inputs are absent"
                );
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
            Self::SubstrateReleaseAuthorization(r) => {
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
                hash(&r.substrate_release_hash)?;
                token(&r.expected_publisher_fingerprint, 256)?;
            }
            Self::SetCompose(r) => {
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
                hash(&r.predecessor_set_hash)?;
                bounded_value(&r.replacement)?;
                let replacement: BundleSetEntry = serde_json::from_value(r.replacement.clone())?;
                anyhow::ensure!(
                    replacement.bundle_name != "core",
                    "Core replacement requires explicit substrate bootstrap authority"
                );
                name(&replacement.bundle_name)?;
                hash(&replacement.generation_hash)?;
                hash(&replacement.publisher_attestation_hash)?;
                anyhow::ensure!(
                    r.substrate_protocol > 0,
                    "substrate protocol must be nonzero"
                );
            }
            Self::SubstrateReleaseFinalize(r) => {
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
                anyhow::ensure!(
                    r.substrate_image_digest
                        .strip_prefix("sha256:")
                        .is_some_and(|digest| hash(digest).is_ok()),
                    "substrate image digest must use sha256"
                );
                anyhow::ensure!(
                    r.substrate_protocol > 0,
                    "substrate protocol must be nonzero"
                );
                hash(&r.substrate_build_accepted_result_hash)?;
                hash(&r.substrate_build_receipt_hash)?;
                name(&r.selected_substrate_product_identity)?;
                hash(&r.selected_substrate_product_witness)?;
                hashes(&r.qualification_evidence_hashes)?;
                anyhow::ensure!(
                    r.qualification_evidence_hashes.len() == 1,
                    "exactly one substrate qualification evidence hash is required"
                );
                hash(&r.core_generation_hash)?;
                hash(&r.core_generation_attestation_hash)?;
                r.release()?;
            }
            Self::GenesisSetCompose(r) => {
                bounded_value(&r.bundle_set)?;
                let set = BundleSet::from_current_value(&r.bundle_set)?;
                anyhow::ensure!(
                    set.entries.iter().any(|entry| entry.bundle_name == "core"),
                    "genesis set requires its exact substrate Core generation"
                );
                name(&r.catalog_namespace)?;
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
            }
            Self::CatalogRequestPublication(r) => {
                hash(&r.bundle_publication_policy_section_digest)?;
                anyhow::ensure!(r.trust_epoch > 0, "trust epoch must be nonzero");
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
        BundleReleaseOperation::CoreSeedBuild(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::AuthorizeCoreSeedRecipe(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::InputInspect(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::AuthorizeBuildRecipe(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::AuthorizeCaptureRecipe(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::AuthorizeSubstrateBuildRecipe(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationBuild(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::RequestTreeSigning(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationCapture(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationQualify(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenerationFinalize(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::RequestAuthorization(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::SubstrateReleaseAuthorization(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::SetCompose(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::SubstrateReleaseFinalize(v) => serde_json::to_value(v)?,
        BundleReleaseOperation::GenesisSetCompose(v) => serde_json::to_value(v)?,
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
    fn authorize_core_seed_recipe<'a>(
        &'a self,
        request: super::core_seed::CoreSeedRecipeRequest,
    ) -> ProducerFuture<'a>;
    fn authorize_build_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeBuildRecipeRequest,
    ) -> ProducerFuture<'a>;
    fn authorize_capture_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeCaptureRecipeRequest,
    ) -> ProducerFuture<'a>;
    fn authorize_substrate_build_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeSubstrateBuildRecipeRequest,
    ) -> ProducerFuture<'a>;
    fn sign_tree<'a>(&'a self, request: RequestTreeSigningRequest) -> ProducerFuture<'a>;
    fn authorize_generation<'a>(
        &'a self,
        request: RequestAuthorizationRequest,
    ) -> ProducerFuture<'a>;
    fn authorize_substrate_release<'a>(
        &'a self,
        request: SubstrateReleaseAuthorizationRequest,
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
    fn authorize_substrate_build_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeSubstrateBuildRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            let result = self
                .post("v1/substrate-build/authorize-recipe", &request)
                .await?;
            request.validate_response(&result, &self.expected_publisher_fingerprint)?;
            Ok(result)
        })
    }
    fn authorize_core_seed_recipe<'a>(
        &'a self,
        request: super::core_seed::CoreSeedRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            let result = self
                .post("v1/substrate-core/authorize-recipe", &request)
                .await?;
            request.validate_response(&result, &self.expected_publisher_fingerprint)?;
            Ok(result)
        })
    }
    fn authorize_build_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeBuildRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            let result = self
                .post("v1/bundle-recipe/authorize-build", &request)
                .await?;
            self.verify_publisher_result(&result)?;
            super::recipe::validate_recipe_response(
                &request,
                &result,
                &self.expected_publisher_fingerprint,
            )?;
            Ok(result)
        })
    }

    fn authorize_capture_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeCaptureRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            let result = self
                .post("v1/bundle-recipe/authorize-capture", &request)
                .await?;
            self.verify_publisher_result(&result)?;
            super::recipe::validate_capture_recipe_response(
                &request,
                &result,
                &self.expected_publisher_fingerprint,
            )?;
            Ok(result)
        })
    }

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

    fn authorize_substrate_release<'a>(
        &'a self,
        request: SubstrateReleaseAuthorizationRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            anyhow::ensure!(
                request.expected_publisher_fingerprint == self.expected_publisher_fingerprint,
                "substrate release authorization requested a different publisher"
            );
            let result = self
                .post("v1/substrate-release/authorize", &request)
                .await?;
            self.verify_publisher_result(&result)?;
            let attestation_hash = result
                .get("substrate_release_attestation_hash")
                .and_then(Value::as_str)
                .context("publisher substrate authorization response has no attestation hash")?;
            hash(attestation_hash)?;
            anyhow::ensure!(
                result.get("schema").and_then(Value::as_str)
                    == Some("ryeos.substrate_release_authorization_result.v1")
                    && result.get("catalog_namespace").and_then(Value::as_str)
                        == Some(request.catalog_namespace.as_str())
                    && result.get("substrate_release_hash").and_then(Value::as_str)
                        == Some(request.substrate_release_hash.as_str()),
                "publisher substrate authorization response disagrees with request"
            );
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

/// Publisher-local verification for the complete set being authorized. This
/// authority cannot admit deployment selections or another publisher's edges.
struct CatalogSigningPolicy<'a> {
    signer: &'a dyn ryeos_state::signer::Signer,
}

impl super::consumer::ConsumerPublicationPolicy for CatalogSigningPolicy<'_> {
    fn verify_publisher_attestation(
        &self,
        attestation: &Attestation,
        expected_claim: &str,
    ) -> anyhow::Result<()> {
        use super::attestation::*;
        let kind = match expected_claim {
            BUNDLE_GENERATION_RELEASE_CLAIM => ReleaseSubjectKind::BundleGeneration,
            BUNDLE_SET_RELEASE_CLAIM => ReleaseSubjectKind::BundleSet,
            SUBSTRATE_RELEASE_CLAIM => ReleaseSubjectKind::SubstrateRelease,
            _ => anyhow::bail!("catalog signing refuses an unsupported release claim"),
        };
        verify_release_attestation(
            attestation,
            &attestation.subject_hash,
            kind,
            self.signer.fingerprint(),
            &self.signer.verifying_key(),
        )
    }

    fn verify_deployment_attestation(&self, _attestation: &Attestation) -> anyhow::Result<()> {
        anyhow::bail!("catalog signing has no node deployment authority")
    }
}

fn verify_catalog_substrate(
    set: &BundleSet,
    objects: &impl super::PublicationObjectReader,
    policy: &impl super::consumer::ConsumerPublicationPolicy,
    binding: &super::ReleasePolicyBinding,
) -> anyhow::Result<()> {
    let attestation = Attestation::from_value(&super::read_exact(
        objects,
        &set.substrate_release_attestation_hash,
    )?)?;
    policy
        .verify_publisher_attestation(&attestation, super::attestation::SUBSTRATE_RELEASE_CLAIM)?;
    let release = ryeos_bundle_publication_contract::SubstrateRelease::from_current_value(
        &super::read_exact(objects, &attestation.subject_hash)?,
    )?;
    anyhow::ensure!(
        release.catalog_namespace == binding.catalog_namespace
            && release.bundle_publication_policy_section_digest
                == binding.bundle_publication_policy_section_digest
            && release.trust_epoch == binding.trust_epoch,
        "substrate release does not match catalog authorization policy"
    );
    let core = set
        .entries
        .iter()
        .find(|entry| entry.bundle_name == "core")
        .context("catalog set omits substrate Core")?;
    anyhow::ensure!(
        release.core_generation_hash == core.generation_hash
            && release.core_generation_attestation_hash == core.publisher_attestation_hash
            && release.substrate_protocol == set.substrate_protocol
            && release.target == set.target,
        "catalog set contradicts its authenticated substrate release"
    );
    Ok(())
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
    fn authorize_substrate_build_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeSubstrateBuildRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            self.tree_publisher
                .authorize_substrate_build_recipe(&request)
        })
    }
    fn authorize_core_seed_recipe<'a>(
        &'a self,
        request: super::core_seed::CoreSeedRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            self.tree_publisher.authorize_core_seed_recipe(&request)
        })
    }
    fn authorize_build_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeBuildRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            self.tree_publisher.authorize_build_recipe(&request)
        })
    }

    fn authorize_capture_recipe<'a>(
        &'a self,
        request: super::recipe::AuthorizeCaptureRecipeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            self.tree_publisher.authorize_capture_recipe(&request)
        })
    }

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

    fn authorize_substrate_release<'a>(
        &'a self,
        request: SubstrateReleaseAuthorizationRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            anyhow::ensure!(
                request.expected_publisher_fingerprint == self.signer.fingerprint(),
                "substrate release authorization requested a different publisher"
            );
            let cas = self.cas()?;
            let release = ryeos_bundle_publication_contract::SubstrateRelease::from_current_value(
                &super::read_exact(cas, &request.substrate_release_hash)?,
            )?;
            anyhow::ensure!(
                release.catalog_namespace == request.catalog_namespace
                    && release.bundle_publication_policy_section_digest
                        == request.bundle_publication_policy_section_digest
                    && release.trust_epoch == request.trust_epoch,
                "substrate release authorization policy coordinates disagree"
            );
            let accepted = ryeos_state::external_content::products::accepted_result::ProductBuildAcceptedResult::from_value(
                &super::read_exact(cas, &release.substrate_build_accepted_result_hash)?,
            )?;
            let core_attestation = Attestation::from_value(&super::read_exact(
                cas,
                &release.core_generation_attestation_hash,
            )?)?;
            super::attestation::verify_release_attestation(
                &core_attestation,
                &release.core_generation_hash,
                super::attestation::ReleaseSubjectKind::BundleGeneration,
                self.signer.fingerprint(),
                &self.signer.verifying_key(),
            )?;
            let binding = super::ReleasePolicyBinding {
                catalog_namespace: request.catalog_namespace.clone(),
                bundle_publication_policy_section_digest: request
                    .bundle_publication_policy_section_digest
                    .clone(),
                trust_epoch: request.trust_epoch,
            };
            let core = super::inspect_bundle_generation(
                &release.core_generation_hash,
                cas,
                self.materialization_proof.as_ref(),
                self.release_evidence_proof.as_ref(),
                &binding,
            )?;
            anyhow::ensure!(
                core.generation().bundle_name == "core"
                    && core.generation().substrate_protocol == release.substrate_protocol
                    && (core.generation().target
                        == ryeos_bundle_publication_contract::BundleTarget::Portable
                        || core.generation().target == release.target),
                "substrate release Core generation does not match the measured substrate"
            );
            self.release_evidence_proof
                .verify_substrate_release_evidence(&release, &accepted, &binding)?;
            let attestation = super::attestation::authorize_substrate_release(
                &release,
                self.signer.as_ref(),
                &lillux::time::iso8601_now(),
            )?;
            let stored = cas.put_object(&attestation.to_value())?;
            Ok(serde_json::json!({
                "schema": "ryeos.substrate_release_authorization_result.v1",
                "catalog_namespace": request.catalog_namespace,
                "substrate_release_hash": request.substrate_release_hash,
                "substrate_release_attestation_hash": stored.hash,
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
            let publisher_policy = CatalogSigningPolicy {
                signer: self.signer.as_ref(),
            };
            // The substrate descriptor is an owning edge, not an opaque hash.
            // Verify it before inspecting/signing any candidate set content.
            verify_catalog_substrate(
                &set,
                cas,
                &publisher_policy,
                &super::ReleasePolicyBinding {
                    catalog_namespace: request.catalog_namespace.clone(),
                    bundle_publication_policy_section_digest: request
                        .bundle_publication_policy_section_digest
                        .clone(),
                    trust_epoch: request.trust_epoch,
                },
            )?;
            super::consumer::verify_prospective_set(
                request.bundle_set_hash.clone(),
                String::new(),
                set.clone(),
                Some(&format!("fp:{}", self.signer.fingerprint())),
                cas,
                &publisher_policy,
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

/// Explicit first-release authority, separate from non-core successors and
/// disaster-recovery restore. It never implies access to publisher keys.
pub trait SubstrateBootstrapAuthority: Send + Sync + 'static {
    fn build_core_seed<'a>(
        &'a self,
        _request: super::core_seed::CoreSeedBuildRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async {
            anyhow::bail!(
                "Core seed production requires an explicitly admitted substrate build executor"
            )
        })
    }
    fn finalize_substrate<'a>(
        &'a self,
        request: SubstrateReleaseFinalizeRequest,
    ) -> ProducerFuture<'a>;
    fn compose_genesis<'a>(&'a self, request: GenesisSetComposeRequest) -> ProducerFuture<'a>;
}

/// Composition-root proof for a measured image and its exact qualified Core
/// generation. Implementations must verify image/target/protocol, Core bytes,
/// accepted build/capture/qualification evidence, publisher attestation and
/// current policy; decoding a descriptor or observing CAS presence is not proof.
pub trait SubstrateBootstrapEvidenceProof: Send + Sync + 'static {
    fn verify_substrate_release(
        &self,
        release: &ryeos_bundle_publication_contract::SubstrateRelease,
        binding: &super::ReleasePolicyBinding,
        objects: &lillux::CasStore,
    ) -> anyhow::Result<()>;
}

/// Composition-root retained-source executor for the fixed Core seed Graph.
pub trait CoreSeedBuildExecutor: Send + Sync + 'static {
    fn build<'a>(&'a self, request: super::core_seed::CoreSeedBuildRequest) -> ProducerFuture<'a>;
}

#[derive(Default)]
pub struct BundleReleaseAuthorityRouter {
    pub build: Option<Arc<dyn BundleBuildAuthority>>,
    pub qualification: Option<Arc<dyn BundleQualificationAuthority>>,
    pub publisher: Option<Arc<dyn BundlePublisherAuthority>>,
    pub cas_composition: Option<Arc<dyn BundleCasCompositionAuthority>>,
    pub recovery: Option<Arc<dyn BundleCatalogRecoveryAuthority>>,
    pub substrate_bootstrap: Option<Arc<dyn SubstrateBootstrapAuthority>>,
}

/// Non-signing bootstrap owner. No default image evidence authority exists.
/// Finalizing stores a descriptor only; publisher authorization is a separate
/// purpose-owned operation. Composing stores an immutable complete set only;
/// the catalog owner still performs the absent-head genesis publication fence.
pub struct AdmittedSubstrateBootstrapAuthority {
    composition: Arc<AdmittedLocalCasCompositionAuthority>,
    evidence: Arc<dyn SubstrateBootstrapEvidenceProof>,
    core_seed_executor: Option<Arc<dyn CoreSeedBuildExecutor>>,
}

impl AdmittedSubstrateBootstrapAuthority {
    pub fn new(
        composition: Arc<AdmittedLocalCasCompositionAuthority>,
        evidence: Arc<dyn SubstrateBootstrapEvidenceProof>,
    ) -> Self {
        Self {
            composition,
            evidence,
            core_seed_executor: None,
        }
    }

    pub fn with_core_seed_executor(mut self, executor: Arc<dyn CoreSeedBuildExecutor>) -> Self {
        self.core_seed_executor = Some(executor);
        self
    }
}

impl SubstrateBootstrapAuthority for AdmittedSubstrateBootstrapAuthority {
    fn build_core_seed<'a>(
        &'a self,
        request: super::core_seed::CoreSeedBuildRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            request.validate()?;
            let executor = self.core_seed_executor.as_ref().context(
                "Core seed production requires an explicitly admitted substrate build executor",
            )?;
            executor.build(request).await
        })
    }
    fn finalize_substrate<'a>(
        &'a self,
        request: SubstrateReleaseFinalizeRequest,
    ) -> ProducerFuture<'a> {
        Box::pin(async move {
            BundleReleaseOperation::SubstrateReleaseFinalize(request.clone()).validate()?;
            let release = request.release()?;
            let authority = self.composition.state_store.pinned_state_authority()?;
            let _guard = authority.acquire_shared_guard()?;
            let cas = authority.cas_store()?;
            self.evidence.verify_substrate_release(
                &release,
                &super::ReleasePolicyBinding {
                    catalog_namespace: request.catalog_namespace,
                    bundle_publication_policy_section_digest: request
                        .bundle_publication_policy_section_digest,
                    trust_epoch: request.trust_epoch,
                },
                &cas,
            )?;
            let hash = cas.store_object(&release.to_value()?)?;
            anyhow::ensure!(
                hash == release.content_hash()?,
                "stored substrate release identity changed"
            );
            Ok(
                serde_json::json!({"schema":"ryeos.substrate_release_finalize_result.v1", "substrate_release_hash":hash}),
            )
        })
    }

    fn compose_genesis<'a>(&'a self, request: GenesisSetComposeRequest) -> ProducerFuture<'a> {
        Box::pin(async move {
            BundleReleaseOperation::GenesisSetCompose(request.clone()).validate()?;
            let set = BundleSet::from_current_value(&request.bundle_set)?;
            let authority = self.composition.state_store.pinned_state_authority()?;
            let _guard = authority.acquire_shared_guard()?;
            let cas = authority.cas_store()?;
            let hash = set.content_hash()?;
            super::consumer::verify_prospective_set(
                hash.clone(),
                String::new(),
                set.clone(),
                Some(&self.composition.expected_publisher),
                &cas,
                self.composition.consumer_policy.as_ref(),
                self.composition.materialization_proof.as_ref(),
                self.composition.release_evidence_proof.as_ref(),
                &super::ReleasePolicyBinding {
                    catalog_namespace: request.catalog_namespace,
                    bundle_publication_policy_section_digest: request
                        .bundle_publication_policy_section_digest,
                    trust_epoch: request.trust_epoch,
                },
            )?;
            let stored = cas.store_object(&set.to_value()?)?;
            anyhow::ensure!(stored == hash, "stored genesis set identity changed");
            Ok(
                serde_json::json!({"schema":"ryeos.genesis_set_compose_result.v1", "bundle_set_hash":hash}),
            )
        })
    }
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
        BundleReleaseOperation::SetCompose(request.clone()).validate()?;
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
            BundleReleaseOperation::CoreSeedBuild(request) => match &self.substrate_bootstrap {
                Some(authority) => authority.build_core_seed(request),
                None => Self::unavailable("core_seed_build", "substrate bootstrap"),
            },
            BundleReleaseOperation::AuthorizeCoreSeedRecipe(request) => match &self.publisher {
                Some(authority) => authority.authorize_core_seed_recipe(request),
                None => Self::unavailable("authorize_core_seed_recipe", "constrained publisher"),
            },
            BundleReleaseOperation::SubstrateReleaseFinalize(request) => {
                match &self.substrate_bootstrap {
                    Some(authority) => authority.finalize_substrate(request),
                    None => Self::unavailable("substrate_release_finalize", "substrate bootstrap"),
                }
            }
            BundleReleaseOperation::GenesisSetCompose(request) => match &self.substrate_bootstrap {
                Some(authority) => authority.compose_genesis(request),
                None => Self::unavailable("genesis_set_compose", "substrate bootstrap"),
            },
            BundleReleaseOperation::InputInspect(request) => match &self.build {
                Some(authority) => authority.inspect(request),
                None => Self::unavailable("input_inspect", "build/inspection"),
            },
            BundleReleaseOperation::GenerationBuild(request) => match &self.build {
                Some(authority) => authority.build(request),
                None => Self::unavailable("generation_build", "build/inspection"),
            },
            BundleReleaseOperation::AuthorizeBuildRecipe(request) => match &self.publisher {
                Some(authority) => authority.authorize_build_recipe(request),
                None => Self::unavailable("authorize_build_recipe", "constrained publisher"),
            },
            BundleReleaseOperation::AuthorizeCaptureRecipe(request) => match &self.publisher {
                Some(authority) => authority.authorize_capture_recipe(request),
                None => Self::unavailable("authorize_capture_recipe", "constrained publisher"),
            },
            BundleReleaseOperation::AuthorizeSubstrateBuildRecipe(request) => match &self.publisher
            {
                Some(authority) => authority.authorize_substrate_build_recipe(request),
                None => {
                    Self::unavailable("authorize_substrate_build_recipe", "constrained publisher")
                }
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
            BundleReleaseOperation::SubstrateReleaseAuthorization(request) => {
                match &self.publisher {
                    Some(authority) => authority.authorize_substrate_release(request),
                    None => Self::unavailable(
                        "substrate_release_authorization",
                        "constrained publisher",
                    ),
                }
            }
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
            BundleReleaseOperation::CoreSeedBuild(r) => Some(r.catalog_namespace.as_str()),
            BundleReleaseOperation::AuthorizeCoreSeedRecipe(r) => {
                Some(r.build().catalog_namespace.as_str())
            }
            BundleReleaseOperation::SubstrateReleaseFinalize(r) => {
                Some(r.catalog_namespace.as_str())
            }
            BundleReleaseOperation::GenesisSetCompose(r) => Some(r.catalog_namespace.as_str()),
            BundleReleaseOperation::AuthorizeBuildRecipe(r) => Some(r.catalog_namespace.as_str()),
            BundleReleaseOperation::AuthorizeCaptureRecipe(r) => Some(r.catalog_namespace.as_str()),
            BundleReleaseOperation::AuthorizeSubstrateBuildRecipe(r) => {
                Some(r.catalog_namespace.as_str())
            }
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

#[cfg(test)]
mod bootstrap_tests {
    use super::*;
    use serde_json::json;

    struct MissingObjects;
    impl super::super::PublicationObjectReader for MissingObjects {
        fn get_object(&self, _hash: &str) -> anyhow::Result<Option<Value>> {
            Ok(None)
        }
    }
    struct RefusePolicy;
    impl super::super::consumer::ConsumerPublicationPolicy for RefusePolicy {
        fn verify_publisher_attestation(
            &self,
            _attestation: &Attestation,
            _claim: &str,
        ) -> anyhow::Result<()> {
            anyhow::bail!("fixture refuses publisher authority")
        }
        fn verify_deployment_attestation(&self, _attestation: &Attestation) -> anyhow::Result<()> {
            anyhow::bail!("fixture refuses deployment authority")
        }
    }

    #[test]
    fn catalog_authorization_requires_present_substrate_edge_before_signing() {
        let request = genesis();
        let set = BundleSet::from_current_value(&request.bundle_set).unwrap();
        let binding = super::super::ReleasePolicyBinding {
            catalog_namespace: request.catalog_namespace,
            bundle_publication_policy_section_digest: request
                .bundle_publication_policy_section_digest,
            trust_epoch: request.trust_epoch,
        };
        assert!(verify_catalog_substrate(&set, &MissingObjects, &RefusePolicy, &binding).is_err());
    }

    #[test]
    fn catalog_authorization_requires_exact_policy_coordinates() {
        let request = json!({
            "catalog_namespace":"official", "channel":"stable", "bundle_name":"web",
            "generation_attestation_hash":"a".repeat(64), "bundle_set_hash":"b".repeat(64),
            "predecessor_publication_attestation_hash":null, "expected_sequence":0
        });
        assert!(
            serde_json::from_value::<CatalogRequestPublicationRequest>(request.clone()).is_err()
        );
        let mut complete = request;
        complete["bundle_publication_policy_section_digest"] = json!("d".repeat(64));
        complete["trust_epoch"] = json!(0);
        let decoded = serde_json::from_value::<CatalogRequestPublicationRequest>(complete).unwrap();
        assert!(
            BundleReleaseOperation::CatalogRequestPublication(decoded)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn substrate_authorization_is_closed_over_hash_policy_and_publisher() {
        let request = SubstrateReleaseAuthorizationRequest {
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "d".repeat(64),
            trust_epoch: 1,
            substrate_release_hash: "e".repeat(64),
            expected_publisher_fingerprint: "f".repeat(64),
        };
        BundleReleaseOperation::SubstrateReleaseAuthorization(request.clone())
            .validate()
            .unwrap();

        let mut invalid = request;
        invalid.trust_epoch = 0;
        assert!(
            BundleReleaseOperation::SubstrateReleaseAuthorization(invalid)
                .validate()
                .is_err()
        );
    }

    fn receipt_stage() -> SubstrateReleaseFinalizeRequest {
        SubstrateReleaseFinalizeRequest {
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "d".repeat(64),
            trust_epoch: 1,
            substrate_image_digest: format!("sha256:{}", "a".repeat(64)),
            substrate_protocol: 1,
            target: ryeos_bundle_publication_contract::BundleTarget::Triple {
                triple: "x86_64-unknown-linux-gnu".into(),
            },
            substrate_build_accepted_result_hash: "b".repeat(64),
            substrate_build_receipt_hash: "c".repeat(64),
            selected_substrate_product_identity: "substrate".into(),
            selected_substrate_product_witness: "e".repeat(64),
            qualification_evidence_hashes: vec!["f".repeat(64)],
            core_generation_hash: "1".repeat(64),
            core_generation_attestation_hash: "2".repeat(64),
        }
    }

    #[test]
    fn receipt_stage_derives_release_without_caller_authored_descriptor() {
        let request = receipt_stage();
        BundleReleaseOperation::SubstrateReleaseFinalize(request.clone())
            .validate()
            .unwrap();
        let release = request.release().unwrap();
        assert_eq!(
            release.substrate_image_digest,
            request.substrate_image_digest
        );
        assert_eq!(release.core_generation_hash, request.core_generation_hash);
        assert_eq!(
            release.substrate_build_receipt_hash,
            request.substrate_build_receipt_hash
        );

        let encoded = serde_json::to_value(request).unwrap();
        assert!(encoded.get("release").is_none());
    }

    #[test]
    fn receipt_stage_rejects_incomplete_or_ambiguous_evidence() {
        let mut request = receipt_stage();
        request.qualification_evidence_hashes.clear();
        assert!(
            BundleReleaseOperation::SubstrateReleaseFinalize(request)
                .validate()
                .is_err()
        );

        let mut request = receipt_stage();
        request.qualification_evidence_hashes = vec!["f".repeat(64), "e".repeat(64)];
        assert!(
            BundleReleaseOperation::SubstrateReleaseFinalize(request)
                .validate()
                .is_err()
        );
    }

    fn genesis() -> GenesisSetComposeRequest {
        GenesisSetComposeRequest {
            bundle_set: json!({
                "schema":ryeos_bundle_publication_contract::BUNDLE_SET_SCHEMA,
                "kind":ryeos_bundle_publication_contract::BUNDLE_SET_KIND,
                "set_name":"initial", "target":{"kind":"triple","triple":"x86_64-unknown-linux-gnu"},
                "substrate_protocol":1, "substrate_release_attestation_hash":"a".repeat(64),
                "entries":[{"bundle_name":"core", "generation_hash":"b".repeat(64), "publisher_attestation_hash":"c".repeat(64)}],
                "migration_requirement":"none"
            }),
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "d".repeat(64),
            trust_epoch: 1,
        }
    }

    #[test]
    fn genesis_requires_substrate_binding_and_core() {
        BundleReleaseOperation::GenesisSetCompose(genesis())
            .validate()
            .unwrap();
        let mut missing_core = genesis();
        missing_core.bundle_set["entries"][0]["bundle_name"] = json!("web");
        assert!(
            BundleReleaseOperation::GenesisSetCompose(missing_core)
                .validate()
                .is_err()
        );
        let mut missing_substrate = genesis();
        missing_substrate
            .bundle_set
            .as_object_mut()
            .unwrap()
            .remove("substrate_release_attestation_hash");
        assert!(
            BundleReleaseOperation::GenesisSetCompose(missing_substrate)
                .validate()
                .is_err()
        );
        let mut request = serde_json::to_value(genesis()).unwrap();
        request["predecessor_set_hash"] = json!("e".repeat(64));
        assert!(serde_json::from_value::<GenesisSetComposeRequest>(request).is_err());
    }

    #[test]
    fn genesis_coordinates_are_idempotent_and_conflicts_are_distinct() {
        let request = genesis();
        let first = BundleSet::from_current_value(&request.bundle_set)
            .unwrap()
            .content_hash()
            .unwrap();
        let retry = BundleSet::from_current_value(&request.bundle_set)
            .unwrap()
            .content_hash()
            .unwrap();
        assert_eq!(first, retry);
        let mut conflicting = request;
        conflicting.bundle_set["substrate_release_attestation_hash"] = json!("f".repeat(64));
        assert_ne!(
            first,
            BundleSet::from_current_value(&conflicting.bundle_set)
                .unwrap()
                .content_hash()
                .unwrap()
        );
        // Composition has no mutable head: catalog publication separately
        // enforces absent-head genesis and rejects a conflicting publication.
    }

    #[test]
    fn ordinary_successor_cannot_replace_core() {
        let request = SetComposeRequest {
            catalog_namespace: "official".into(),
            bundle_publication_policy_section_digest: "d".repeat(64),
            trust_epoch: 1,
            predecessor_set_hash: "e".repeat(64),
            replacement: genesis().bundle_set["entries"][0].clone(),
            substrate_protocol: 1,
        };
        assert!(
            BundleReleaseOperation::SetCompose(request)
                .validate()
                .unwrap_err()
                .to_string()
                .contains("Core replacement")
        );
    }

    #[tokio::test]
    async fn bootstrap_is_not_available_through_default_release_authority() {
        let error = BundleReleaseAuthorityRouter::default()
            .execute(BundleReleaseOperation::GenesisSetCompose(genesis()))
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("explicitly composed substrate bootstrap")
        );
    }
}
