//! Operator-owned external-content import, binding, and integrity operations.
//!
//! The API layer authenticates ordinary signed service execution; this module
//! keeps ambient import and general binding local-operator-only. Exact owned
//! product operations also admit configured remote operators. Both resolve the
//! node-owned import policy and reuse the same meaning-blind state primitives;
//! no product selector grants another owner's bytes or ambient filesystem access.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use ryeos_state::external_content::products::transfer::ProductWitnessSource;
use serde::{Deserialize, Serialize};

use crate::handler_context::HandlerContext;
use crate::node_policy::sections::object_closure::NodeObjectClosurePolicy;
use crate::state::AppState;

pub mod product_build;
pub mod product_composition;
pub mod product_qualification;
pub mod product_receipt;
pub mod products;
mod retained_binding;
mod retained_product;
mod retained_result;

const BINDING_HEAD_NAMESPACE: &str = ryeos_state::objects::EXTERNAL_CONTENT_BINDING_HEAD_NAMESPACE;

/// Retire every predecessor external-content binding head while the node is
/// stopped. Manifest-schema cuts change the binding coordinate itself; old
/// active heads cannot remain roots under the new decoder and are never
/// translated.
pub fn discard_binding_heads_offline(
    config: &crate::config::Config,
    dry_run: bool,
) -> anyhow::Result<usize> {
    let _state_lock = crate::state_lock::StateLock::acquire(&crate::state_lock::default_lock_path(
        &config.app_root,
    ))
    .context("external-content binding reset requires the daemon to be stopped")?;
    let runtime_state_dir = config.runtime_state_dir();
    let identity = crate::identity::NodeIdentity::load(&config.node_signing_key_path)
        .context("load node identity for external-content binding reset")?;
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    let state = ryeos_state::StateDb::open_for_projection_rebuild(
        &runtime_state_dir,
        std::sync::Arc::new(trust),
    )
    .context("open pinned state authority for external-content binding reset")?;
    let authority = state.pinned_authority()?;
    let guard = authority.acquire_exclusive_guard(!dry_run)?;
    state.discard_external_content_binding_heads(&guard, dry_run)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportShape {
    File,
    Tree,
}

/// The two mechanically distinct retained-storage implementations. The
/// resulting manifest remains self-describing; this selection controls only
/// how the operator capture writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStorage {
    Content,
    LargeContent,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemImportRequest {
    pub root: String,
    pub path: String,
    pub shape: ImportShape,
    pub storage: ImportStorage,
    pub maximum_bytes: u64,
    #[serde(default)]
    pub expected_file_sha256: Option<String>,
}

/// Source selection is explicit and closed. Neither a snapshot hash nor a
/// named root can be substituted for the other source's authorization.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportRequest {
    Filesystem(FilesystemImportRequest),
    RetainedResult(RetainedResultImportRequest),
    RetainedBinding(RetainedBindingImportRequest),
    RetainedProduct(RetainedProductImportRequest),
}

/// Fresh import capability for bytes retained by one exact published product
/// witness. Publication grants no consumer access; ordinary bind still decides.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedProductImportRequest {
    pub witness_hash: String,
    pub witness_source: ProductWitnessSource,
    pub maximum_bytes: u64,
}

/// Reuse the complete exact manifest of a currently active local binding.
/// The binding owns shape/storage/manifest identity; callers cannot restate
/// them or use an old import receipt to authorize another consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedBindingImportRequest {
    pub binding_hash: String,
    pub maximum_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedResultImportRequest {
    pub chain_root_id: String,
    pub thread_id: String,
    pub result_project_snapshot_hash: String,
    pub path: String,
    pub shape: ImportShape,
    pub storage: ImportStorage,
    pub maximum_bytes: u64,
    #[serde(default)]
    pub expected_file_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportResponse {
    pub staging_id: String,
    pub request_digest: String,
    pub manifest_hash: String,
    pub manifest_kind: String,
    pub entry_count: usize,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindRequest {
    pub staging_id: String,
    pub request_digest: String,
    pub manifest_hash: String,
    pub consumer_ref: String,
    pub consumer_kind: BindConsumerKind,
    #[serde(default)]
    pub project_snapshot_hash: Option<String>,
    /// Logical project coordinate retained by the shared snapshot context.
    /// Resolution reads only the materialized snapshot; this path is neither
    /// opened nor committed to binding identity.
    #[serde(default)]
    pub project_path: Option<std::path::PathBuf>,
    /// Complete root product-selection batch used to reconstruct an exact D1
    /// consumer before binding this independently staged literal manifest.
    /// Absence retains the ordinary unselected literal-binding path.
    #[serde(default)]
    pub product_selections:
        Option<Vec<ryeos_state::external_content::products::composition::ProductSelection>>,
    /// Exact currently admitted operator that owns every selected product.
    /// The configured local operator still authorizes and publishes the bind.
    #[serde(default)]
    pub product_owner_principal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindConsumerKind {
    InstalledBundle,
    PinnedProject,
}

impl BindRequest {
    pub fn validate_consumer_request(&self) -> anyhow::Result<()> {
        match (&self.product_selections, &self.product_owner_principal) {
            (Some(selections), Some(owner)) => {
                product_composition::validate_selection_batch(selections)?;
                let fingerprint = owner.strip_prefix("fp:").ok_or_else(|| {
                    anyhow::anyhow!("selected binding product_owner_principal is not canonical")
                })?;
                if !lillux::valid_hash(fingerprint)
                    || fingerprint.bytes().any(|byte| byte.is_ascii_uppercase())
                {
                    anyhow::bail!("selected binding product_owner_principal is not canonical");
                }
            }
            (Some(_), None) => {
                anyhow::bail!("selected binding requires an exact product_owner_principal")
            }
            (None, Some(_)) => {
                anyhow::bail!("unselected binding cannot carry product_owner_principal")
            }
            (None, None) => {}
        }
        match self.consumer_kind {
            BindConsumerKind::InstalledBundle
                if self.project_snapshot_hash.is_none() && self.project_path.is_none() =>
            {
                Ok(())
            }
            BindConsumerKind::PinnedProject
                if self.project_snapshot_hash.is_some() && self.project_path.is_some() =>
            {
                let snapshot = self
                    .project_snapshot_hash
                    .as_deref()
                    .expect("checked pinned-project snapshot");
                if !lillux::valid_hash(snapshot)
                    || snapshot.bytes().any(|byte| byte.is_ascii_uppercase())
                {
                    anyhow::bail!(
                        "pinned-project binding project_snapshot_hash is not a canonical digest"
                    );
                }
                Ok(())
            }
            BindConsumerKind::InstalledBundle => anyhow::bail!(
                "installed-bundle binding cannot carry project snapshot or path fields"
            ),
            BindConsumerKind::PinnedProject => anyhow::bail!(
                "pinned-project binding requires project_snapshot_hash and project_path"
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindResponse {
    pub binding_subject_id: String,
    pub binding_id: String,
    pub binding_hash: String,
    pub manifest_hash: String,
    pub consumer_ref: String,
    pub publisher_fingerprint: String,
    pub idempotent: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequest {
    pub binding_subject_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseResponse {
    pub binding_subject_id: String,
    pub binding_id: String,
    pub binding_hash: String,
    pub manifest_hash: String,
    pub consumer_ref: String,
    pub publisher_fingerprint: String,
    pub idempotent: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindingIntegrityFinding {
    pub binding_subject_id: String,
    pub binding_hash: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScrubResponse {
    pub objects_verified: usize,
    pub bytes_verified: u64,
    pub object_findings: Vec<ryeos_state::LargeObjectIntegrityFinding>,
    pub bindings_verified: usize,
    pub binding_findings: Vec<BindingIntegrityFinding>,
    pub abandoned_staging_removed: usize,
}

pub async fn import(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ImportRequest,
) -> anyhow::Result<ImportResponse> {
    match request {
        ImportRequest::Filesystem(request) => import_filesystem(state, context, request).await,
        ImportRequest::RetainedResult(request) => retained_result::import(state, context, request),
        ImportRequest::RetainedBinding(request) => {
            tokio::task::spawn_blocking(move || retained_binding::import(state, context, request))
                .await
                .context("retained binding import worker stopped")?
        }
        ImportRequest::RetainedProduct(request) => {
            tokio::task::spawn_blocking(move || retained_product::import(state, context, request))
                .await
                .context("retained product import worker stopped")?
        }
    }
}

async fn import_filesystem(
    state: Arc<AppState>,
    context: HandlerContext,
    request: FilesystemImportRequest,
) -> anyhow::Result<ImportResponse> {
    let operator_fingerprint =
        crate::operator_authority::require_local_configured_operator(&state, &context)?;
    validate_relative_path(&request.path)?;
    if request.maximum_bytes == 0 {
        bail!("external-content import maximum_bytes must be positive");
    }
    if request.shape != ImportShape::File && request.expected_file_sha256.is_some() {
        bail!("expected_file_sha256 is valid only for a file import");
    }
    if let Some(hash) = request.expected_file_sha256.as_deref()
        && !lillux::valid_hash(hash)
    {
        bail!("expected_file_sha256 is not a canonical sha256 digest");
    }
    let policy = state.node_policy.require::<
        crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord,
    >()?;
    let root_policy = policy
        .roots
        .get(&request.root)
        .ok_or_else(|| anyhow::anyhow!("external-content import root is not admitted"))?;
    if request.maximum_bytes > policy.limits.max_total_bytes {
        bail!("external-content import maximum_bytes exceeds node policy");
    }
    let source_root = lillux::PinnedDirectory::open(&root_policy.path)?
        .ok_or_else(|| anyhow::anyhow!("external-content import root is unavailable"))?;
    let (root_device, root_inode) = source_root.device_inode()?;
    if root_policy.containing_device != root_device || root_policy.root_inode != root_inode {
        bail!("external-content import root filesystem identity changed");
    }
    let request_digest = import_request_digest(
        &request,
        &policy.limits,
        root_device,
        root_inode,
        state.ignore_matcher.as_ref(),
    )?;
    let publication_key =
        ryeos_state::DurableCasPublicationKey::external_content_import(&request_digest)?;

    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let large_store = authority.large_object_store()?;
    let maximum_entries = if request.shape == ImportShape::File {
        1
    } else {
        policy.limits.max_entries
    };
    require_import_store_capacity(
        "external-content CAS",
        cas.filesystem_capacity()?,
        policy.limits.minimum_free_bytes,
        request.maximum_bytes,
        maximum_entries,
    )?;
    if request.storage == ImportStorage::LargeContent {
        require_import_store_capacity(
            "external-content large store",
            large_store.filesystem_capacity()?,
            policy.limits.minimum_free_bytes,
            request.maximum_bytes,
            maximum_entries,
        )?;
    }
    if request.storage == ImportStorage::LargeContent {
        let maximum_store_after = large_store
            .total_stored_bytes()?
            .checked_add(request.maximum_bytes)
            .ok_or_else(|| anyhow::anyhow!("external-content store budget overflow"))?;
        if maximum_store_after > policy.limits.store_budget_bytes {
            bail!("external-content import would exceed the node large-store budget");
        }
    }

    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire external-content write permit: {error}")
        })?;
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            &guard,
            &operator_fingerprint,
            "external-content-import",
            &publication_key,
            None,
        )?;
    let response = match request.storage {
        ImportStorage::Content => capture_content_import(
            &request,
            &policy.limits,
            &source_root,
            root_device,
            state.ignore_matcher.as_ref(),
            &guard,
            &cas,
            &mut stage,
            request_digest,
        )?,
        ImportStorage::LargeContent => capture_large_import(
            &request,
            &policy.limits,
            &source_root,
            root_device,
            state.ignore_matcher.as_ref(),
            &guard,
            &cas,
            &large_store,
            &mut stage,
            request_digest,
        )?,
    };
    source_root.ensure_path_binding()?;
    drop(stage);
    drop(_permit);
    drop(guard);
    Ok(response)
}

/// Import one already-verified managed-activation component from a node-owned
/// descriptor-pinned staging directory. The caller must have resolved the
/// signed activation config and authenticated the configured operator; this
/// helper accepts no ambient root name, host path, or caller-selected limit.
pub fn import_managed_activation_component(
    state: &AppState,
    operator_fingerprint: &str,
    activation: &crate::managed_external_content::ResolvedManagedExternalContentActivation,
    component: &crate::managed_external_content::ResolvedManagedActivationComponent,
    source_root: &lillux::PinnedDirectory,
    staged_name: &str,
) -> anyhow::Result<ImportResponse> {
    use crate::managed_external_content::ManagedComponentStorage;

    let admitted_component = activation.component(&component.recipe.id)?;
    if !lillux::valid_hash(operator_fingerprint)
        || admitted_component.recipe != component.recipe
        || admitted_component.expected_manifest_hash != component.expected_manifest_hash
        || admitted_component.capture_bounds != component.capture_bounds
    {
        bail!("managed external-content import authority is inconsistent");
    }
    let maximum_bytes = component.capture_bounds.maximum_total_bytes;
    let maximum_file_bytes = component.capture_bounds.maximum_file_bytes;
    let maximum_depth = component.capture_bounds.maximum_depth;
    let maximum_entries = component.capture_bounds.maximum_entries;
    let expected_file_sha256 = component.expected_file_sha256.clone();
    validate_relative_path(staged_name)?;
    let import_policy = state.node_policy.require::<
        crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord,
    >()?;
    let policy = import_policy.managed_activation.require_enabled()?;
    activation.document.validate_portable()?;
    let request = FilesystemImportRequest {
        root: "managed-activation-staging".to_owned(),
        path: staged_name.to_owned(),
        shape: match component.declaration_kind {
            ryeos_engine::external_content::ExternalContentKind::File => ImportShape::File,
            ryeos_engine::external_content::ExternalContentKind::Tree => ImportShape::Tree,
        },
        storage: match component.recipe.storage {
            ManagedComponentStorage::Content => ImportStorage::Content,
            ManagedComponentStorage::LargeContent => ImportStorage::LargeContent,
        },
        maximum_bytes,
        expected_file_sha256,
    };
    if maximum_depth > import_policy.limits.max_depth
        || maximum_entries > import_policy.limits.max_entries
        || maximum_file_bytes > import_policy.limits.max_file_bytes
        || maximum_bytes > import_policy.limits.max_total_bytes
    {
        bail!("managed external-content component exceeds current node import policy");
    }
    let limits = crate::node_policy::sections::external_content::ExternalContentImportLimits {
        max_depth: maximum_depth,
        max_entries: maximum_entries,
        max_file_bytes: maximum_file_bytes,
        max_total_bytes: maximum_bytes,
        store_budget_bytes: policy
            .store_budget_bytes
            .min(import_policy.limits.store_budget_bytes),
        minimum_free_bytes: policy
            .minimum_free_bytes
            .max(import_policy.limits.minimum_free_bytes),
    };
    let policy_digest = crate::managed_external_content_operation::managed_policy_digest(
        &import_policy.limits,
        policy,
    )?;
    let request_digest = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema":"ryeos.managed_external_content_import.v3",
        "activation_program_digest":activation.activation_program_digest,
        "consumer_ref":activation.document.consumer_ref,
        "component":component.recipe,
        "derived_consumer_authority":{
            "kind":component.declaration_kind,
            "manifest_hash":component.expected_manifest_hash,
            "manifest_kind":component.expected_manifest_kind,
        },
        "policy_digest":policy_digest,
        "capture_floor_rules":ryeos_state::project_sync::durable_content_capture_floor_rules(),
        "configured_ignore_patterns":state.ignore_matcher.canonical_patterns(),
    }))?;
    let publication_key =
        ryeos_state::DurableCasPublicationKey::external_content_import(&request_digest)?;
    let (root_device, _) = source_root.device_inode()?;
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let large_store = authority.large_object_store()?;
    require_import_store_capacity(
        "managed external-content CAS",
        cas.filesystem_capacity()?,
        limits.minimum_free_bytes,
        maximum_bytes,
        maximum_entries,
    )?;
    if request.storage == ImportStorage::LargeContent {
        require_import_store_capacity(
            "managed external-content large store",
            large_store.filesystem_capacity()?,
            limits.minimum_free_bytes,
            maximum_bytes,
            maximum_entries,
        )?;
    }
    if request.storage == ImportStorage::LargeContent
        && large_store
            .total_stored_bytes()?
            .checked_add(maximum_bytes)
            .ok_or_else(|| anyhow::anyhow!("managed external-content store budget overflow"))?
            > limits.store_budget_bytes
    {
        bail!("managed external-content import would exceed the node large-store budget");
    }

    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire managed external-content write permit: {error}")
        })?;
    let mut stage = authority
        .require_recovery()?
        .begin_durable_cas_upload_admitted(
            &guard,
            operator_fingerprint,
            "managed-external-content-import",
            &publication_key,
            None,
        )?;
    let response = match request.storage {
        ImportStorage::Content => capture_content_import(
            &request,
            &limits,
            source_root,
            root_device,
            state.ignore_matcher.as_ref(),
            &guard,
            &cas,
            &mut stage,
            request_digest,
        )?,
        ImportStorage::LargeContent => capture_large_import(
            &request,
            &limits,
            source_root,
            root_device,
            state.ignore_matcher.as_ref(),
            &guard,
            &cas,
            &large_store,
            &mut stage,
            request_digest,
        )?,
    };
    if response.manifest_hash != component.expected_manifest_hash
        || response.manifest_kind != component.expected_manifest_kind
    {
        bail!("managed external-content component differs from its signed manifest commitment");
    }
    source_root.ensure_path_binding()?;
    drop(stage);
    drop(_permit);
    drop(guard);
    Ok(response)
}

// A captured entry can transiently require a staged file plus its immutable
// object, sidecar, lock, and hash-shard directories. Reserve conservatively on
// each actual destination filesystem; deduplication only improves the margin.
const IMPORT_ALLOCATION_UNITS_PER_ENTRY: u64 = 8;
const IMPORT_FIXED_ALLOCATION_UNITS: u64 = 64;

fn require_import_store_capacity(
    label: &str,
    capacity: lillux::FilesystemCapacity,
    minimum_free_bytes: u64,
    maximum_bytes: u64,
    maximum_entries: usize,
) -> anyhow::Result<()> {
    let maximum_entries = u64::try_from(maximum_entries)?;
    let file_identities = maximum_entries
        .checked_mul(IMPORT_ALLOCATION_UNITS_PER_ENTRY)
        .and_then(|value| value.checked_add(IMPORT_FIXED_ALLOCATION_UNITS))
        .ok_or_else(|| anyhow::anyhow!("{label} file-identity reserve overflow"))?;
    let allocation_overhead = file_identities
        .checked_mul(capacity.allocation_unit_bytes)
        .ok_or_else(|| anyhow::anyhow!("{label} allocation reserve overflow"))?;
    let required_free = minimum_free_bytes
        .checked_add(maximum_bytes)
        .and_then(|value| value.checked_add(allocation_overhead))
        .ok_or_else(|| anyhow::anyhow!("{label} free-space requirement overflow"))?;
    if capacity.available_bytes < required_free {
        bail!(
            "{label} requires {required_free} available bytes, observed {}",
            capacity.available_bytes
        );
    }
    if capacity.available_files < file_identities {
        bail!(
            "{label} requires {file_identities} available file identities, observed {}",
            capacity.available_files
        );
    }
    Ok(())
}

pub async fn bind(
    state: Arc<AppState>,
    context: HandlerContext,
    request: BindRequest,
) -> anyhow::Result<BindResponse> {
    let operator_fingerprint =
        crate::operator_authority::require_local_configured_operator(&state, &context)?;
    ensure_unselected_bind_request(&request, BindConsumerKind::InstalledBundle)?;
    let consumer = resolve_installed_external_content_consumer(
        &state,
        &request.consumer_ref,
        &request.manifest_hash,
    )?;
    bind_authorized(state, operator_fingerprint, request.into(), consumer).await
}

/// Complete a project binding after the API layer has materialized the exact
/// retained snapshot and admitted its source closure. The resolution is the
/// common pre-realization document used again by launch admission; no live
/// project path participates in this authorization.
pub async fn bind_pinned_project(
    state: Arc<AppState>,
    context: HandlerContext,
    request: BindRequest,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<BindResponse> {
    let operator_fingerprint =
        crate::operator_authority::require_local_configured_operator(&state, &context)?;
    ensure_unselected_bind_request(&request, BindConsumerKind::PinnedProject)?;
    let project_snapshot_hash = request
        .project_snapshot_hash
        .as_deref()
        .expect("validated pinned-project request");
    let consumer = resolve_project_external_content_consumer(
        &state,
        resolution,
        &request.consumer_ref,
        project_snapshot_hash,
        &request.manifest_hash,
    )?;
    bind_authorized(state, operator_fingerprint, request.into(), consumer).await
}

fn ensure_unselected_bind_request(
    request: &BindRequest,
    expected_kind: BindConsumerKind,
) -> anyhow::Result<()> {
    request.validate_consumer_request()?;
    if request.consumer_kind != expected_kind {
        bail!("external-content binding used the wrong consumer preparation owner");
    }
    if request.product_selections.is_some() {
        bail!("selected external-content binding requires exact D1 preparation");
    }
    Ok(())
}

/// Bind an independently staged literal manifest against an exact selected
/// consumer. The selections are authenticated again against the prepared D1;
/// an absent, partial, or different batch is never interpreted as D0 authority.
pub async fn bind_selected_literal_resolution(
    state: Arc<AppState>,
    context: HandlerContext,
    request: BindRequest,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
) -> anyhow::Result<BindResponse> {
    crate::operator_authority::require_local_configured_operator(&state, &context)?;
    let subject = selected_bind_subject(&request)?;
    let selections = request
        .product_selections
        .as_deref()
        .expect("selected binding subject requires selections");
    let product_owner = request
        .product_owner_principal
        .as_deref()
        .expect("selected binding validation requires product owner");
    crate::operator_authority::admitted_operator_authority_for_principal(&state, product_owner)?;
    product_composition::verify_recovered_selections(
        &state,
        resolution,
        &subject,
        product_owner,
        selections,
    )?;
    let publication = BindPublicationRequest {
        staging_id: request.staging_id,
        request_digest: request.request_digest,
        manifest_hash: request.manifest_hash,
        consumer_ref: request.consumer_ref,
    };
    bind_prepared_resolution(
        state,
        context,
        resolution,
        &subject,
        publication,
        Some(request.consumer_kind),
    )
    .await
}

fn selected_bind_subject(
    request: &BindRequest,
) -> anyhow::Result<ryeos_engine::contracts::SubjectResolutionAuthority> {
    request.validate_consumer_request()?;
    if request.product_selections.is_none() {
        bail!("selected literal binding has no product selections");
    }
    Ok(match request.consumer_kind {
        BindConsumerKind::InstalledBundle => {
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
        }
        BindConsumerKind::PinnedProject => {
            ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash: request
                    .project_snapshot_hash
                    .clone()
                    .expect("validated pinned-project request"),
            }
        }
    })
}

async fn bind_prepared_resolution(
    state: Arc<AppState>,
    context: HandlerContext,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    subject: &ryeos_engine::contracts::SubjectResolutionAuthority,
    request: BindPublicationRequest,
    expected_kind: Option<BindConsumerKind>,
) -> anyhow::Result<BindResponse> {
    let operator = crate::operator_authority::require_local_configured_operator(&state, &context)?;
    bind_prepared_resolution_authorized(
        state,
        operator,
        resolution,
        subject,
        request,
        expected_kind,
    )
    .await
}

/// Shared binding mechanics after the distinct public-local or exact-product
/// entrypoint has authenticated its owner. Never infer this authority from a
/// manifest, staged bytes, or a caller-supplied principal.
async fn bind_prepared_resolution_authorized(
    state: Arc<AppState>,
    operator: String,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    subject: &ryeos_engine::contracts::SubjectResolutionAuthority,
    request: BindPublicationRequest,
    expected_kind: Option<BindConsumerKind>,
) -> anyhow::Result<BindResponse> {
    let consumer_authority =
        crate::external_content_admission::consumer_authority(resolution, subject)?;
    ensure_bind_consumer_kind(expected_kind, &consumer_authority)?;
    let consumer = resolve_external_content_consumer_from_resolution(
        &state,
        resolution,
        consumer_authority,
        &request.manifest_hash,
    )?;
    bind_authorized(state, operator, request, consumer).await
}

fn ensure_bind_consumer_kind(
    expected_kind: Option<BindConsumerKind>,
    consumer_authority: &ryeos_state::objects::ExternalContentConsumerAuthority,
) -> anyhow::Result<()> {
    match (expected_kind, consumer_authority) {
        (None, _)
        | (
            Some(BindConsumerKind::InstalledBundle),
            ryeos_state::objects::ExternalContentConsumerAuthority::InstalledBundle { .. },
        )
        | (
            Some(BindConsumerKind::PinnedProject),
            ryeos_state::objects::ExternalContentConsumerAuthority::PinnedProject { .. },
        ) => {}
        _ => {
            bail!("selected external-content consumer source differs from the bind request")
        }
    }
    Ok(())
}

/// Bind an explicitly composed manifest using the same source-inclusive selected
/// resolution which will be checked by execution. Bundle consumers must not be
/// re-resolved here: that would discard their admitted product selections.
pub(super) async fn bind_selected_product_resolution(
    state: Arc<AppState>,
    context: HandlerContext,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    subject: &ryeos_engine::contracts::SubjectResolutionAuthority,
    imported: ImportResponse,
) -> anyhow::Result<BindResponse> {
    let operator = crate::operator_authority::require_admitted_operator(&state, &context)?;
    let request = BindPublicationRequest {
        staging_id: imported.staging_id,
        request_digest: imported.request_digest,
        manifest_hash: imported.manifest_hash,
        consumer_ref: resolution.root.resolved_ref.clone(),
    };
    bind_prepared_resolution_authorized(state, operator, resolution, subject, request, None).await
}

/// Bind one component after a managed-activation caller has authenticated the
/// configured operator and retained the exact signed activation program. This
/// is not an API authorization boundary; it is the shared state transition
/// behind the separately authorized generic activation service.
pub async fn bind_managed_activation_component(
    state: Arc<AppState>,
    operator_fingerprint: String,
    activation: &crate::managed_external_content::ResolvedManagedExternalContentActivation,
    request: BindRequest,
) -> anyhow::Result<BindResponse> {
    if !lillux::valid_hash(&operator_fingerprint)
        || request.consumer_ref != activation.document.consumer_ref
        || request.consumer_kind != BindConsumerKind::InstalledBundle
    {
        bail!("managed external-content binding authority is inconsistent");
    }
    ensure_unselected_bind_request(&request, BindConsumerKind::InstalledBundle)?;
    let consumer = resolve_installed_external_content_consumer(
        &state,
        &request.consumer_ref,
        &request.manifest_hash,
    )?;
    bind_authorized(state, operator_fingerprint, request.into(), consumer).await
}

/// Publication consumes an already checked consumer, not public source-location
/// controls. Keep those controls out of the shared mutation owner.
struct BindPublicationRequest {
    staging_id: String,
    request_digest: String,
    manifest_hash: String,
    consumer_ref: String,
}

impl From<BindRequest> for BindPublicationRequest {
    fn from(request: BindRequest) -> Self {
        Self {
            staging_id: request.staging_id,
            request_digest: request.request_digest,
            manifest_hash: request.manifest_hash,
            consumer_ref: request.consumer_ref,
        }
    }
}

async fn bind_authorized(
    state: Arc<AppState>,
    operator_fingerprint: String,
    request: BindPublicationRequest,
    consumer: ResolvedConsumer,
) -> anyhow::Result<BindResponse> {
    if !lillux::valid_hash(&request.request_digest) || !lillux::valid_hash(&request.manifest_hash) {
        bail!("external-content bind request contains a non-canonical digest");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let current_binding_epoch = state
        .state_store
        .with_state_db(|db| db.external_content_binding_schema_epoch())?;
    if current_binding_epoch != Some(ryeos_state::objects::EXTERNAL_CONTENT_BINDING_SCHEMA_EPOCH) {
        // A fresh node has no binding epoch until its first bind. Publish that
        // epoch under the same barrier -> exclusive-CAS order used by
        // maintenance, before this request acquires its ordinary shared CAS
        // guard. Predecessor epochs still fail closed and require the explicit
        // stopped-node reset ceremony.
        let _permit = state
            .write_barrier
            .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
            .map_err(|error| {
                anyhow::anyhow!("cannot acquire external-content epoch write permit: {error}")
            })?;
        let epoch_guard = authority.acquire_exclusive_guard(true)?;
        state
            .state_store
            .with_state_db(|db| db.ensure_current_external_content_binding_epoch(&epoch_guard))?;
    }
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let store = authority.large_object_store()?;
    let manifest_value = cas
        .get_object(&request.manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("external-content bind target is absent"))?;
    let manifest_kind = manifest_value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("external-content bind target has no manifest kind"))?;
    if consumer.authority.consumer_ref() != request.consumer_ref {
        bail!("resolved external-content consumer contradicts the bind request");
    }
    let target_node_fingerprint = state.identity.fingerprint().to_owned();
    let authorizer_grant_digest = crate::operator_authority::admitted_operator_authority_digest(
        &state,
        &operator_fingerprint,
    )?;
    let binding_subject_id =
        ryeos_state::objects::ExternalContentBinding::derive_binding_subject_id(
            &request.manifest_hash,
            manifest_kind,
            &consumer.authority,
            &target_node_fingerprint,
        )?;
    let publication_key =
        ryeos_state::DurableCasPublicationKey::external_content_import(&request.request_digest)?;
    let recovery = authority.require_recovery()?;
    // Take the generic publication barrier before the per-stage lock and keep
    // that order through the complete bind. Imports use the same order while
    // creating/capturing a stage. Besides fencing the binding head used by the
    // idempotent proof below, this prevents duplicate bind retries from each
    // retaining one stage lock while waiting on the other's publication
    // permit.
    let _publication_permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire external-content binding write permit: {error}")
        })?;
    let mut stage = recovery.open_durable_cas_upload_admitted(
        &guard,
        &request.staging_id,
        &operator_fingerprint,
    )?;
    stage.ensure_publication_contract(&publication_key, None)?;

    if let Some(current) = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, &binding_subject_id))?
    {
        let current_value = cas
            .get_object(&current.target_hash)?
            .ok_or_else(|| anyhow::anyhow!("current external-content binding is absent"))?;
        let current_binding =
            ryeos_state::objects::ExternalContentBinding::from_value(&current_value)?;
        if current_binding.state == ryeos_state::objects::ExternalContentBindingState::Active
            && current_binding.binding_subject_id == binding_subject_id
            && current_binding.manifest_hash == request.manifest_hash
            && current_binding.manifest_kind == manifest_kind
            && current_binding.consumer == consumer.authority
            && current_binding.target_node_fingerprint == target_node_fingerprint
            && current_binding.authorized_by == operator_fingerprint
            && current_binding.authorizer_grant_digest == authorizer_grant_digest
        {
            settle_current_binding_receipt(&mut stage, &guard, &current.target_hash)?;
            return Ok(BindResponse {
                binding_subject_id,
                binding_id: current_binding.binding_id,
                binding_hash: current.target_hash,
                manifest_hash: request.manifest_hash,
                consumer_ref: consumer.authority.consumer_ref().to_owned(),
                publisher_fingerprint: consumer.authority.publisher_fingerprint().to_owned(),
                idempotent: true,
            });
        }
    }
    if let Some(binding_hash) = stage.admitted_target_hash() {
        let current = state.state_store.with_state_db(|db| {
            db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, &binding_subject_id)
        })?;
        if current.as_ref().map(|head| head.target_hash.as_str()) != Some(binding_hash) {
            bail!("admitted external-content binding receipt is not the current signed head");
        }
        let admitted_value = cas
            .get_object(binding_hash)?
            .ok_or_else(|| anyhow::anyhow!("admitted external-content binding is absent"))?;
        let admitted_binding =
            ryeos_state::objects::ExternalContentBinding::from_value(&admitted_value)?;
        if !binding_authorizes_consumer(
            &admitted_binding,
            &binding_subject_id,
            &request.manifest_hash,
            &consumer.authority,
            &target_node_fingerprint,
            manifest_kind,
        )? || admitted_binding.authorized_by != operator_fingerprint
            || admitted_binding.authorizer_grant_digest != authorizer_grant_digest
        {
            bail!("admitted import receipt belongs to a predecessor binding authorization");
        }
        return Ok(BindResponse {
            binding_subject_id,
            binding_id: admitted_binding.binding_id,
            binding_hash: binding_hash.to_owned(),
            manifest_hash: request.manifest_hash,
            consumer_ref: consumer.authority.consumer_ref().to_owned(),
            publisher_fingerprint: consumer.authority.publisher_fingerprint().to_owned(),
            idempotent: true,
        });
    }

    stage.ensure_protects_object(&request.manifest_hash)?;
    let closure = match manifest_kind {
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
            let manifest =
                ryeos_state::objects::ExternalContentManifestObject::from_value(&manifest_value)?;
            if consumer.declaration_kind
                == ryeos_engine::external_content::ExternalContentKind::File
                && !manifest.is_file_shaped()
            {
                bail!("consumer declares a file realization but the staged manifest is a tree");
            }
            let verified =
                ryeos_state::VerifiedExternalContentClosure::load(&cas, &request.manifest_hash)?;
            let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                &cas,
                [request.manifest_hash.clone()],
                state
                    .node_policy
                    .require::<NodeObjectClosurePolicy>()?
                    .closure_limits()?,
            )?;
            if !closure.is_complete() || verified.manifest() != &manifest {
                bail!("external-content binding closure is incomplete");
            }
            closure
        }
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
            let manifest = ryeos_state::objects::ExternalLargeContentManifestObject::from_value(
                &manifest_value,
            )?;
            let grant_max_total_bytes = consumer.grant_max_total_bytes.ok_or_else(|| {
                anyhow::anyhow!("consumer kind has no signed large-content grant")
            })?;
            if manifest.total_bytes > grant_max_total_bytes {
                bail!("staged manifest exceeds the consumer kind's signed large-content ceiling");
            }
            if consumer.declaration_kind
                == ryeos_engine::external_content::ExternalContentKind::File
                && !manifest.is_file_shaped()
            {
                bail!("consumer declares a file realization but the staged manifest is a tree");
            }
            for entry in &manifest.entries {
                if entry.file_sha256.is_some() {
                    stage.ensure_protects_large_object(
                        entry.file_sha256.as_deref().expect("checked large entry"),
                    )?;
                    store.verify_manifest_commitment(entry)?;
                }
            }
            let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                &cas,
                [request.manifest_hash.clone()],
                state
                    .node_policy
                    .require::<NodeObjectClosurePolicy>()?
                    .closure_limits()?,
            )?;
            if !closure.is_complete() {
                bail!("external-content binding closure is incomplete");
            }
            closure
        }
        other => bail!("external-content bind target has unsupported manifest kind `{other}`"),
    };
    let binding = ryeos_state::objects::ExternalContentBinding::active(
        request.manifest_hash.clone(),
        manifest_kind.to_owned(),
        consumer.authority.clone(),
        target_node_fingerprint,
        operator_fingerprint,
        authorizer_grant_digest,
    )?;
    // The shared CAS guard protects synchronous writes until a complete root
    // is published. Never root a not-yet-written object, nor duplicate its
    // transitive closure in the upload receipt: GC already follows typed edges.
    let binding_hash = cas.store_object(&binding.to_value()?)?;
    let binding_closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [binding_hash.clone()],
        state
            .node_policy
            .require::<NodeObjectClosurePolicy>()?
            .closure_limits()?,
    )?;
    if !binding_closure.is_complete() {
        bail!("external-content binding closure is incomplete");
    }
    for hash in &binding_closure.large_object_hashes {
        stage.ensure_protects_large_object(hash)?;
    }
    stage.protect_cas_closure(&guard, [binding_hash.as_str()], std::iter::empty())?;
    debug_assert!(closure.object_hashes.contains(&request.manifest_hash));

    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
        db.ensure_current_external_content_binding_epoch(&guard)?;
        let current = db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, &binding_subject_id)?;
        if let Some(current) = current.as_ref()
            && current.target_hash == binding_hash
        {
            return Ok(());
        }
        db.advance_generic_head_ref(
            BINDING_HEAD_NAMESPACE,
            &binding_subject_id,
            &binding_hash,
            current.as_ref().map(|head| head.target_hash.as_str()),
            &signer,
            &guard,
        )
    })?;
    if let Err(error) = stage.finish_admitted(&guard, &binding_hash) {
        tracing::warn!(%error, staging_id = %request.staging_id, "binding head published while import receipt remained retryable");
    }
    Ok(BindResponse {
        binding_subject_id,
        binding_id: binding.binding_id,
        binding_hash,
        manifest_hash: request.manifest_hash,
        consumer_ref: consumer.authority.consumer_ref().to_owned(),
        publisher_fingerprint: consumer.authority.publisher_fingerprint().to_owned(),
        idempotent: false,
    })
}

/// The caller has verified the exact current binding under the publication
/// barrier. Settle only its presented, principal-bound upload capability.
/// Import request identity commits acquisition, not a consumer: other stages
/// with the same request may bind the same immutable bytes to different Tools
/// or project generations. Never bulk-settle those stages as duplicate binds.
/// Abandoned stages remain subject to the existing explicit maintenance policy.
fn settle_current_binding_receipt(
    stage: &mut ryeos_state::DurableCasUploadStage,
    guard: &ryeos_state::CasMutationGuard,
    binding_hash: &str,
) -> anyhow::Result<()> {
    if let Some(admitted) = stage.admitted_target_hash() {
        if admitted != binding_hash {
            bail!(
                "external-content import receipt target contradicts the current idempotent binding"
            );
        }
        return Ok(());
    }
    // A fresh retry did not upload this already-current binding object.
    // Protect its exact root before settlement; GC traverses its typed closure.
    stage.protect_cas_closure(guard, std::iter::once(binding_hash), std::iter::empty())?;
    stage.finish_admitted(guard, binding_hash)
}

pub async fn scrub(state: Arc<AppState>, context: HandlerContext) -> anyhow::Result<ScrubResponse> {
    crate::operator_authority::require_local_configured_operator(&state, &context)?;
    let authority = state.state_store.pinned_state_authority()?;
    let _guard = authority.acquire_shared_guard()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire external-content write permit: {error}")
        })?;
    let cas = authority.cas_store()?;
    let store = authority.large_object_store()?;
    let report = store.scrub_all()?;
    let mut bindings_verified = 0usize;
    let mut binding_findings = Vec::new();
    let heads = state
        .state_store
        .with_state_db(|db| db.list_generic_head_refs(BINDING_HEAD_NAMESPACE))?;
    for head in heads {
        let checked = (|| -> anyhow::Result<()> {
            let value = cas
                .get_object(&head.target_hash)?
                .ok_or_else(|| anyhow::anyhow!("binding head target is absent"))?;
            let binding = ryeos_state::objects::ExternalContentBinding::from_value(&value)?;
            if head.namespace != BINDING_HEAD_NAMESPACE || head.name != binding.binding_subject_id {
                bail!("binding head coordinates contradict the retained binding");
            }
            if binding.target_node_fingerprint != state.identity.fingerprint() {
                bail!("binding belongs to a different target node");
            }
            if binding.state == ryeos_state::objects::ExternalContentBindingState::Active {
                require_current_binding_authorizer(&state, &binding)?;
                let closure =
                    ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                        &cas,
                        [head.target_hash.clone()],
                        state
                            .node_policy
                            .require::<NodeObjectClosurePolicy>()?
                            .closure_limits()?,
                    )?;
                if !closure.is_complete() {
                    bail!("active external-content binding closure is incomplete");
                }
                let manifest_value = cas
                    .get_object(&binding.manifest_hash)?
                    .ok_or_else(|| anyhow::anyhow!("active binding manifest is absent"))?;
                if manifest_value
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    != Some(binding.manifest_kind.as_str())
                {
                    bail!("active binding manifest kind changed");
                }
                match binding.manifest_kind.as_str() {
                    ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND => {
                        ryeos_state::VerifiedExternalContentClosure::load(
                            &cas,
                            &binding.manifest_hash,
                        )?;
                    }
                    ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => {
                        let manifest =
                            ryeos_state::objects::ExternalLargeContentManifestObject::from_value(
                                &manifest_value,
                            )?;
                        for entry in &manifest.entries {
                            if entry.file_sha256.is_some() {
                                store.verify_manifest_commitment(entry)?;
                            }
                        }
                    }
                    _ => bail!("active binding names an unsupported manifest kind"),
                }
            }
            Ok(())
        })();
        match checked {
            Ok(()) => bindings_verified = bindings_verified.saturating_add(1),
            Err(error) => binding_findings.push(BindingIntegrityFinding {
                binding_subject_id: head.name,
                binding_hash: head.target_hash,
                error: format!("{error:#}"),
            }),
        }
    }
    let abandoned_staging_removed = store.sweep_abandoned_staging()?;
    Ok(ScrubResponse {
        objects_verified: report.objects_verified,
        bytes_verified: report.bytes_verified,
        object_findings: report.findings,
        bindings_verified,
        binding_findings,
        abandoned_staging_removed,
    })
}

pub async fn release(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ReleaseRequest,
) -> anyhow::Result<ReleaseResponse> {
    let operator_fingerprint =
        crate::operator_authority::require_local_configured_operator(&state, &context)?;
    if !lillux::valid_hash(&request.binding_subject_id)
        || request
            .binding_subject_id
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    {
        bail!("external-content release binding_subject_id is not a canonical digest");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let worker_state = state.clone();
    let binding_subject_id = request.binding_subject_id;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (command_tx, command_rx) = std::sync::mpsc::sync_channel(1);
    let worker = tokio::task::spawn_blocking(move || {
        let guard = match authority.acquire_shared_guard() {
            Ok(guard) => guard,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("{error:#}")));
                return Err(error).context("acquire external-content release authority");
            }
        };
        let _ = ready_tx.send(Ok(()));
        match command_rx.recv() {
            Ok(ReleaseWorkerCommand::Run) => release_under_guard(
                &worker_state,
                &authority,
                &guard,
                &binding_subject_id,
                &operator_fingerprint,
            ),
            Ok(ReleaseWorkerCommand::Abort) | Err(_) => {
                bail!("external-content release aborted before publication")
            }
        }
    });
    ready_rx
        .await
        .context("external-content release worker stopped before acquiring authority")?
        .map_err(anyhow::Error::msg)?;
    if let Err(error) = state
        .write_barrier
        .quiesce(lillux::time::Duration::from_secs(30))
        .await
        .context("quiesce launches before external-content binding release")
    {
        let _ = command_tx.send(ReleaseWorkerCommand::Abort);
        let _ = worker.await;
        return Err(error);
    }
    let _resume = ResumeWriteBarrier(state.write_barrier.clone());
    command_tx
        .send(ReleaseWorkerCommand::Run)
        .context("external-content release worker stopped before publication")?;
    worker
        .await
        .context("external-content release worker panicked")?
}

enum ReleaseWorkerCommand {
    Run,
    Abort,
}

fn release_under_guard(
    state: &AppState,
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    binding_subject_id: &str,
    operator_fingerprint: &str,
) -> anyhow::Result<ReleaseResponse> {
    let cas = authority.cas_store()?;
    let current = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, binding_subject_id))?
        .ok_or_else(|| anyhow::anyhow!("external-content binding does not exist"))?;
    let value = cas
        .get_object(&current.target_hash)?
        .ok_or_else(|| anyhow::anyhow!("external-content binding head target is absent"))?;
    let active = ryeos_state::objects::ExternalContentBinding::from_value(&value)?;
    if active.binding_subject_id != binding_subject_id {
        bail!("external-content binding head identity is inconsistent");
    }
    if active.state == ryeos_state::objects::ExternalContentBindingState::Released {
        return Ok(ReleaseResponse {
            binding_subject_id: active.binding_subject_id,
            binding_id: active.binding_id,
            binding_hash: current.target_hash,
            manifest_hash: active.manifest_hash,
            consumer_ref: active.consumer.consumer_ref().to_owned(),
            publisher_fingerprint: active.consumer.publisher_fingerprint().to_owned(),
            idempotent: true,
        });
    }
    let authorizer_grant_digest =
        crate::operator_authority::admitted_operator_authority_digest(state, operator_fingerprint)?;
    let released = ryeos_state::objects::ExternalContentBinding::released_from(
        &active,
        operator_fingerprint.to_owned(),
        authorizer_grant_digest,
    )?;
    let mut stage = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(guard, "external-content-binding-release")?;
    let released_hash = stage.store_object_admitted(guard, &cas, &released.to_value()?)?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
        db.advance_generic_head_ref(
            BINDING_HEAD_NAMESPACE,
            binding_subject_id,
            &released_hash,
            Some(&current.target_hash),
            &signer,
            guard,
        )
    })?;
    if let Err(error) = stage.finish_admitted(guard) {
        tracing::warn!(%error, %binding_subject_id, "released binding head published while temporary root remained recoverable");
    }
    Ok(ReleaseResponse {
        binding_subject_id: released.binding_subject_id,
        binding_id: released.binding_id,
        binding_hash: released_hash,
        manifest_hash: released.manifest_hash,
        consumer_ref: released.consumer.consumer_ref().to_owned(),
        publisher_fingerprint: released.consumer.publisher_fingerprint().to_owned(),
        idempotent: false,
    })
}

struct ResumeWriteBarrier(Arc<crate::write_barrier::WriteBarrier>);

impl Drop for ResumeWriteBarrier {
    fn drop(&mut self) {
        self.0.resume();
    }
}

#[derive(Debug, thiserror::Error)]
#[error("external-content consumer has no active operator binding")]
pub struct BindingNotActive;

pub fn require_active_binding(
    state: &AppState,
    cas: &lillux::CasStore,
    manifest_hash: &str,
    consumer: &ryeos_state::objects::ExternalContentConsumerAuthority,
) -> anyhow::Result<ryeos_state::objects::ExternalContentBinding> {
    let binding = require_active_binding_from_store(
        &state.state_store,
        cas,
        manifest_hash,
        consumer,
        state.identity.fingerprint(),
    )?;
    require_current_binding_authorizer(state, &binding)?;
    Ok(binding)
}

pub fn require_current_binding_authorizer(
    state: &AppState,
    binding: &ryeos_state::objects::ExternalContentBinding,
) -> anyhow::Result<()> {
    // General binding writers remain local-only; exact owned product writers
    // can retain a configured remote grant. The existing grant digest pins
    // its class, source origin and scope generation without a second authority.
    let current_digest = crate::operator_authority::admitted_operator_authority_digest(
        state,
        &binding.authorized_by,
    )?;
    if current_digest != binding.authorizer_grant_digest {
        bail!("external-content binding authorizer grant changed");
    }
    Ok(())
}

/// Verify the exact active binding using only the state authority that owns
/// it. This keeps recovery validation independent of the broader daemon
/// composition while preserving the same binding checks used at launch.
pub fn require_active_binding_from_store(
    state_store: &crate::state_store::StateStore,
    cas: &lillux::CasStore,
    manifest_hash: &str,
    consumer: &ryeos_state::objects::ExternalContentConsumerAuthority,
    target_node_fingerprint: &str,
) -> anyhow::Result<ryeos_state::objects::ExternalContentBinding> {
    active_binding_from_store(
        state_store,
        cas,
        manifest_hash,
        consumer,
        target_node_fingerprint,
    )?
    .map(|(_, binding)| binding)
    .ok_or_else(|| BindingNotActive.into())
}

/// Inspect one exact consumer binding without changing its head or retaining
/// any CAS root. Absence and a well-formed released head are ordinary
/// not-ready states; corrupt or contradictory retained authority is an error.
pub fn active_binding_from_store(
    state_store: &crate::state_store::StateStore,
    cas: &lillux::CasStore,
    manifest_hash: &str,
    consumer: &ryeos_state::objects::ExternalContentConsumerAuthority,
    target_node_fingerprint: &str,
) -> anyhow::Result<Option<(String, ryeos_state::objects::ExternalContentBinding)>> {
    let manifest_kind = cas
        .get_object(manifest_hash)?
        .and_then(|value| {
            value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| anyhow::anyhow!("external-content binding manifest is absent or untyped"))?;
    let binding_subject_id =
        ryeos_state::objects::ExternalContentBinding::derive_binding_subject_id(
            manifest_hash,
            &manifest_kind,
            consumer,
            target_node_fingerprint,
        )?;
    let Some(head) = state_store.with_state_db(|db| {
        db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, &binding_subject_id)
    })?
    else {
        return Ok(None);
    };
    let value = cas
        .get_object(&head.target_hash)?
        .ok_or_else(|| anyhow::anyhow!("external-content binding head target is absent"))?;
    let binding = ryeos_state::objects::ExternalContentBinding::from_value(&value)?;
    if !binding_authorizes_consumer(
        &binding,
        &binding_subject_id,
        manifest_hash,
        consumer,
        target_node_fingerprint,
        &manifest_kind,
    )? {
        return Ok(None);
    }
    Ok(Some((head.target_hash, binding)))
}

fn binding_authorizes_consumer(
    binding: &ryeos_state::objects::ExternalContentBinding,
    binding_subject_id: &str,
    manifest_hash: &str,
    consumer: &ryeos_state::objects::ExternalContentConsumerAuthority,
    target_node_fingerprint: &str,
    manifest_kind: &str,
) -> anyhow::Result<bool> {
    if binding.binding_subject_id != binding_subject_id
        || binding.manifest_hash != manifest_hash
        || &binding.consumer != consumer
        || binding.target_node_fingerprint != target_node_fingerprint
        || binding.manifest_kind != manifest_kind
    {
        bail!("external-content binding does not authorize this consumer");
    }
    match binding.state {
        ryeos_state::objects::ExternalContentBindingState::Active => Ok(true),
        ryeos_state::objects::ExternalContentBindingState::Released => Ok(false),
    }
}

struct ResolvedConsumer {
    authority: ryeos_state::objects::ExternalContentConsumerAuthority,
    declaration_kind: ryeos_engine::external_content::ExternalContentKind,
    grant_max_total_bytes: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
fn capture_content_import(
    request: &FilesystemImportRequest,
    limits: &crate::node_policy::sections::external_content::ExternalContentImportLimits,
    source_root: &lillux::PinnedDirectory,
    root_device: u64,
    configured_ignore: &ryeos_state::ignore::IgnoreMatcher,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::CasStore,
    stage: &mut ryeos_state::DurableCasUploadStage,
    request_digest: String,
) -> anyhow::Result<ImportResponse> {
    let mut budget = ryeos_state::LaunchCaptureBudget::bounded(
        limits.max_depth.min(ryeos_state::MAX_CAPTURE_DEPTH),
        limits.max_entries.min(ryeos_state::MAX_CAPTURE_ENTRIES),
        limits
            .max_file_bytes
            .min(request.maximum_bytes)
            .min(ryeos_state::MAX_CAPTURE_FILE_BYTES),
        request.maximum_bytes.min(ryeos_state::MAX_CAPTURE_BYTES),
    )?;
    let capture_policy =
        ryeos_state::ExternalCapturePolicy::new(request.path.clone(), configured_ignore)?;
    let mut sink = DurableContentSink { _guard: guard, cas };
    let manifest = match request.shape {
        ImportShape::Tree => {
            let target = open_admitted_source_tree(source_root, &request.path, root_device)?;
            let manifest =
                ryeos_state::capture_tree(&target, &[], &capture_policy, &mut budget, &mut sink)?;
            target.ensure_path_binding()?;
            manifest
        }
        ImportShape::File => {
            let (parent, name) = open_file_parent(source_root, &request.path)?;
            let entry = parent
                .entry_no_follow(OsStr::new(name))?
                .ok_or_else(|| anyhow::anyhow!("external-content source file is unavailable"))?;
            if entry.entry_type != lillux::PinnedEntryType::Regular
                || entry.containing_device != root_device
            {
                bail!("external-content source file is not an admitted regular inode");
            }
            let manifest = ryeos_state::capture_file_at(
                &parent,
                OsStr::new(name),
                &request.path,
                &mut budget,
                &mut sink,
            )?;
            if let Some(expected) = request.expected_file_sha256.as_deref()
                && manifest.entries[0].blob_hash.as_deref() != Some(expected)
            {
                bail!(
                    "external-content source file expected {expected}, observed {}",
                    manifest.entries[0]
                        .blob_hash
                        .as_deref()
                        .unwrap_or("<missing>")
                );
            }
            parent.ensure_path_binding()?;
            manifest
        }
    };
    let manifest_hash = cas.store_object(&serde_json::to_value(&manifest)?)?;
    let verified = ryeos_state::VerifiedExternalContentClosure::load(cas, &manifest_hash)?;
    if verified.manifest() != &manifest {
        bail!("stored content manifest differs from captured value");
    }
    stage.protect_cas_closure(guard, [manifest_hash.as_str()], std::iter::empty())?;
    Ok(ImportResponse {
        staging_id: stage.staging_id().to_owned(),
        request_digest,
        manifest_hash,
        manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn capture_large_import(
    request: &FilesystemImportRequest,
    limits: &crate::node_policy::sections::external_content::ExternalContentImportLimits,
    source_root: &lillux::PinnedDirectory,
    root_device: u64,
    configured_ignore: &ryeos_state::ignore::IgnoreMatcher,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::CasStore,
    large_store: &ryeos_state::LargeObjectStore,
    stage: &mut ryeos_state::DurableCasUploadStage,
    request_digest: String,
) -> anyhow::Result<ImportResponse> {
    let bounds = ryeos_state::LargeContentCaptureBounds {
        max_depth: limits.max_depth,
        max_entries: limits
            .max_entries
            .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_ENTRIES),
        max_file_bytes: limits.max_file_bytes.min(request.maximum_bytes),
        max_total_bytes: request.maximum_bytes,
    };
    let capture_policy = ryeos_state::LargeContentCapturePolicy::new(
        request.path.clone(),
        configured_ignore,
        bounds,
    )?;
    let mut sink = DurableLargeSink {
        guard,
        cas,
        stage,
        store: large_store,
    };
    let manifest = match request.shape {
        ImportShape::Tree => {
            let target = open_admitted_source_tree(source_root, &request.path, root_device)?;
            let manifest = ryeos_state::capture_large_tree(&target, &capture_policy, &mut sink)?;
            target.ensure_path_binding()?;
            manifest
        }
        ImportShape::File => {
            let (parent, file, source_identity) =
                open_pinned_source_file(source_root, &request.path, root_device)?;
            let manifest = ryeos_state::capture_large_file(
                file,
                source_identity,
                &request.path,
                request.expected_file_sha256.as_deref(),
                &capture_policy,
                &mut sink,
            )?;
            parent.ensure_path_binding()?;
            manifest
        }
    };
    for entry in &manifest.entries {
        if let Some(file_sha256) = entry.file_sha256.as_deref() {
            large_store.verify_manifest_commitment(entry)?;
            sink.stage.ensure_protects_large_object(file_sha256)?;
        }
    }
    let manifest_hash = cas.store_object(&manifest.to_value()?)?;
    let loaded = ryeos_state::objects::load_if_large_content_manifest(cas, &manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("stored large-content manifest changed kind"))?;
    if loaded != manifest {
        bail!("stored large-content manifest differs from captured value");
    }
    sink.stage
        .protect_cas_closure(guard, [manifest_hash.as_str()], std::iter::empty())?;
    Ok(ImportResponse {
        staging_id: sink.stage.staging_id().to_owned(),
        request_digest,
        manifest_hash,
        manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND.to_owned(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
    })
}

fn open_pinned_source_file(
    source_root: &lillux::PinnedDirectory,
    relative: &str,
    root_device: u64,
) -> anyhow::Result<(
    lillux::PinnedDirectory,
    std::fs::File,
    ryeos_state::PinnedLargeObjectSourceIdentity,
)> {
    let (parent, name) = open_file_parent(source_root, relative)?;
    let entry = parent
        .entry_no_follow(OsStr::new(name))?
        .ok_or_else(|| anyhow::anyhow!("external-content source file is unavailable"))?;
    if entry.entry_type != lillux::PinnedEntryType::Regular
        || entry.containing_device != root_device
    {
        bail!("external-content source file is not an admitted regular inode");
    }
    let file = parent
        .open_regular(OsStr::new(name), false)?
        .ok_or_else(|| anyhow::anyhow!("external-content source file vanished"))?;
    let observed = lillux::observe_open_regular_file(&file)?;
    if !observed.matches_directory_entry(&entry) {
        bail!("external-content source file changed inode during admission");
    }
    Ok((
        parent,
        file,
        ryeos_state::PinnedLargeObjectSourceIdentity {
            containing_device: entry.containing_device,
            inode: entry.inode,
            size: observed.size(),
        },
    ))
}

fn open_admitted_source_tree(
    source_root: &lillux::PinnedDirectory,
    relative: &str,
    root_device: u64,
) -> anyhow::Result<lillux::PinnedDirectory> {
    let target = open_directory_relative(source_root, relative)?;
    let (target_device, _) = target.device_inode()?;
    if target_device != root_device {
        bail!("external-content source tree crossed the admitted root filesystem");
    }
    Ok(target)
}

struct DurableContentSink<'a> {
    // This synchronous capture has no per-file acknowledgement. The guard
    // excludes GC until the verified completed manifest is durably rooted.
    _guard: &'a ryeos_state::CasMutationGuard,
    cas: &'a lillux::CasStore,
}

impl ryeos_state::ExternalContentBlobSink for DurableContentSink<'_> {
    fn store_file(
        &mut self,
        file: std::fs::File,
        path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        let outcome = self.cas.put_blob_from_open_regular_bounded(
            file,
            std::path::Path::new(path),
            ryeos_state::MAX_CAPTURE_FILE_BYTES,
        )?;
        if outcome.size != expected_size {
            bail!("external-content source file changed size during capture");
        }
        Ok((outcome.hash, outcome.size))
    }
}

fn resolve_installed_external_content_consumer(
    state: &AppState,
    requested_ref: &str,
    manifest_hash: &str,
) -> anyhow::Result<ResolvedConsumer> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(requested_ref)
        .map_err(|error| anyhow::anyhow!("invalid consumer ref: {error}"))?;
    if canonical.to_string() != requested_ref {
        bail!("external-content consumer ref must be canonical");
    }
    let resolution =
        state
            .engine
            .effective_resolution_output(ryeos_engine::engine::EffectiveItemRequest {
                item_ref: canonical,
                expected_kind: None,
                project_root: None,
                subject_resolution_authority:
                    ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
            })?;
    if resolution.effective_trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle
        || resolution.root.source_space != ryeos_engine::contracts::ItemSpace::Bundle
        || !matches!(
            &resolution.root.source_root,
            ryeos_engine::contracts::ItemSourceRoot::Bundle { .. }
        )
    {
        bail!("external-content consumer must be a trusted installed-bundle item");
    }
    let publisher_fingerprint = resolution
        .root
        .signer_fingerprint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("trusted external-content consumer has no signer"))?;
    let consumer = ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
        resolution.root.resolved_ref.clone(),
        publisher_fingerprint,
    )?;
    resolve_external_content_consumer_from_resolution(state, &resolution, consumer, manifest_hash)
}

fn resolve_project_external_content_consumer(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    requested_ref: &str,
    project_snapshot_hash: &str,
    manifest_hash: &str,
) -> anyhow::Result<ResolvedConsumer> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(requested_ref)
        .map_err(|error| anyhow::anyhow!("invalid consumer ref: {error}"))?;
    if canonical.to_string() != requested_ref || resolution.root.resolved_ref != requested_ref {
        bail!("resolved project consumer does not match the canonical requested ref");
    }
    if resolution.root.source_space != ryeos_engine::contracts::ItemSpace::Project
        || !matches!(
            &resolution.root.source_root,
            ryeos_engine::contracts::ItemSourceRoot::Project
        )
        || !matches!(
            resolution.effective_trust_class,
            ryeos_engine::resolution::TrustClass::TrustedProject
                | ryeos_engine::resolution::TrustClass::UntrustedProject
        )
    {
        bail!("project external-content consumer must resolve from the pinned project");
    }
    let consumer = crate::external_content_admission::consumer_authority(
        resolution,
        &ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
            snapshot_hash: project_snapshot_hash.to_owned(),
        },
    )?;
    resolve_external_content_consumer_from_resolution(state, resolution, consumer, manifest_hash)
}

fn resolve_external_content_consumer_from_resolution(
    state: &AppState,
    resolution: &ryeos_engine::resolution::ResolutionOutput,
    authority: ryeos_state::objects::ExternalContentConsumerAuthority,
    manifest_hash: &str,
) -> anyhow::Result<ResolvedConsumer> {
    if resolution
        .composed
        .derived
        .contains_key(ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY)
    {
        bail!("external-content consumer was already realized before binding");
    }
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(authority.consumer_ref())?;
    let external_contract = state
        .engine
        .kinds
        .get(&canonical.kind)
        .and_then(|kind| kind.external_content_contract())
        .ok_or_else(|| anyhow::anyhow!("consumer kind has no signed external-content contract"))?;
    let grant_max_total_bytes = external_contract.large_content.as_ref().map(|large| {
        large
            .max_total_bytes
            .unwrap_or(ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES)
    });
    let declarations = ryeos_engine::external_content::external_content_declarations_for_binding(
        resolution,
        Some(external_contract),
        ryeos_engine::external_content::declaring_authority(resolution)?,
    )?
    .ok_or_else(|| anyhow::anyhow!("consumer does not declare external content"))?;
    let declaration = declarations.iter().find(|declaration| {
        declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && declaration.digest.as_deref() == Some(manifest_hash)
            && declaration.locator.is_none()
    });
    let declaration = declaration
        .ok_or_else(|| anyhow::anyhow!("consumer does not declare the staged manifest digest"))?;
    Ok(ResolvedConsumer {
        authority,
        declaration_kind: declaration.kind,
        grant_max_total_bytes,
    })
}

struct DurableLargeSink<'a> {
    guard: &'a ryeos_state::CasMutationGuard,
    cas: &'a lillux::CasStore,
    stage: &'a mut ryeos_state::DurableCasUploadStage,
    store: &'a ryeos_state::LargeObjectStore,
}

impl ryeos_state::ExternalLargeContentSink for DurableLargeSink<'_> {
    fn store_large_file(
        &mut self,
        file: std::fs::File,
        identity: ryeos_state::PinnedLargeObjectSourceIdentity,
        relative_path: &str,
        expected_sha256: Option<&str>,
    ) -> anyhow::Result<ryeos_state::IngestedLargeObject> {
        let ingested =
            self.store
                .ingest_open_regular(file, identity, relative_path, expected_sha256)?;
        self.stage
            .protect_large_object_hash(self.guard, &ingested.file_sha256)?;
        Ok(ingested)
    }

    fn store_content_file(
        &mut self,
        file: std::fs::File,
        relative_path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        let capacity = usize::try_from(expected_size)
            .map_err(|_| anyhow::anyhow!("large-content file {relative_path} is too large"))?;
        let mut bytes = Vec::with_capacity(capacity);
        use std::io::Read as _;
        file.take(expected_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected_size {
            bail!("large-content file {relative_path} changed size during CAS ingest");
        }
        // Like ordinary content capture, the held guard protects these bytes
        // until the completed manifest is rooted. Explicit large-file roots
        // remain in the receipt because binding proves their import authority.
        let hash = self.cas.store_blob(&bytes)?;
        Ok((hash, expected_size))
    }
}

fn import_request_digest(
    request: &FilesystemImportRequest,
    limits: &crate::node_policy::sections::external_content::ExternalContentImportLimits,
    root_device: u64,
    root_inode: u64,
    configured_ignore: &ryeos_state::ignore::IgnoreMatcher,
) -> anyhow::Result<String> {
    let canonical = lillux::canonical_json(&serde_json::json!({
        "request": {
            "path": request.path,
            "shape": match request.shape { ImportShape::File => "file", ImportShape::Tree => "tree" },
            "storage": match request.storage {
                ImportStorage::Content => "content",
                ImportStorage::LargeContent => "large_content",
            },
            "maximum_bytes": request.maximum_bytes,
            "expected_file_sha256": request.expected_file_sha256,
        },
        "selected_root_identity": {
            "logical_name": request.root,
            "containing_device": root_device,
            "root_inode": root_inode,
        },
        "limits": limits,
        "capture_floor_rules": ryeos_state::project_sync::durable_content_capture_floor_rules(),
        "configured_ignore_patterns": configured_ignore.canonical_patterns(),
    }))?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn validate_relative_path(value: &str) -> anyhow::Result<()> {
    ryeos_state::objects::validate_canonical_project_relative_path(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synchronous_import_roots_completed_manifest_not_each_blob() {
        for storage in [ImportStorage::Content, ImportStorage::LargeContent] {
            let temp = tempfile::tempdir().unwrap();
            let state_dir = temp.path().join("state");
            let source = temp.path().join("source");
            std::fs::create_dir_all(source.join("tree")).unwrap();
            for n in 0..16 {
                std::fs::write(source.join(format!("tree/file-{n}")), format!("file {n}")).unwrap();
            }
            let recovery = ryeos_state::RecoveryStore::from_runtime_state_dir(&state_dir).unwrap();
            let guard = ryeos_state::CasMutationGuard::acquire_shared(&state_dir).unwrap();
            let cas = lillux::CasStore::new(state_dir.join("cas"));
            let key =
                ryeos_state::DurableCasPublicationKey::external_content_import(&"a".repeat(64))
                    .unwrap();
            let mut stage = recovery
                .begin_durable_cas_upload_admitted(
                    &guard,
                    &"b".repeat(64),
                    "test-import",
                    &key,
                    None,
                )
                .unwrap();
            let root = lillux::PinnedDirectory::open(&source).unwrap().unwrap();
            let (device, _) = root.device_inode().unwrap();
            let request = FilesystemImportRequest {
                root: "fixture".into(),
                path: "tree".into(),
                shape: ImportShape::Tree,
                storage,
                maximum_bytes: 4096,
                expected_file_sha256: None,
            };
            let limits =
                crate::node_policy::sections::external_content::ExternalContentImportLimits {
                    max_depth: 8,
                    max_entries: 32,
                    max_file_bytes: 4096,
                    max_total_bytes: 4096,
                    store_budget_bytes: 8192,
                    minimum_free_bytes: 0,
                };
            let ignore = ryeos_state::ignore::IgnoreMatcher::from_config(
                &ryeos_state::ignore::IgnoreConfig { patterns: vec![] },
            )
            .unwrap();
            // Before a completed root is published, an interrupted synchronous
            // write is unacknowledged garbage, not a missing operational root.
            let orphan = cas.store_blob(b"interrupted capture").unwrap();
            assert!(
                recovery
                    .inspect_staged_cas_root_hashes_read_only()
                    .unwrap()
                    .blob_hashes
                    .is_empty()
            );
            let imported = match storage {
                ImportStorage::Content => capture_content_import(
                    &request,
                    &limits,
                    &root,
                    device,
                    &ignore,
                    &guard,
                    &cas,
                    &mut stage,
                    "a".repeat(64),
                )
                .unwrap(),
                ImportStorage::LargeContent => {
                    let runtime = lillux::PinnedDirectory::open(&state_dir).unwrap().unwrap();
                    let large =
                        ryeos_state::LargeObjectStore::open_or_create_under(&runtime).unwrap();
                    capture_large_import(
                        &request,
                        &limits,
                        &root,
                        device,
                        &ignore,
                        &guard,
                        &cas,
                        &large,
                        &mut stage,
                        "a".repeat(64),
                    )
                    .unwrap()
                }
            };
            let roots = recovery.inspect_staged_cas_root_hashes_read_only().unwrap();
            assert_eq!(roots.object_hashes, vec![imported.manifest_hash.clone()]);
            assert!(roots.blob_hashes.is_empty());
            let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
                &cas,
                roots.object_hashes,
                ryeos_state::object_closure::ObjectClosureLimits::default(),
            )
            .unwrap();
            assert!(closure.is_complete());
            assert_eq!(closure.blob_hashes.len(), 16);
            assert!(!closure.blob_hashes.contains(&orphan));
            let id = stage.staging_id().to_owned();
            drop(stage);
            recovery
                .open_durable_cas_upload_admitted(&guard, &id, &"b".repeat(64))
                .unwrap()
                .ensure_protects_object(&imported.manifest_hash)
                .unwrap();
        }
    }

    #[test]
    fn binding_retry_settles_only_its_presented_receipt_for_shared_content() {
        let temp = tempfile::tempdir().unwrap();
        let recovery = ryeos_state::RecoveryStore::from_runtime_state_dir(temp.path()).unwrap();
        let guard = ryeos_state::CasMutationGuard::acquire_shared(temp.path()).unwrap();
        let owner = "a".repeat(64);
        let key = ryeos_state::DurableCasPublicationKey::external_content_import(&"b".repeat(64))
            .unwrap();
        let first_binding = "c".repeat(64);
        let second_binding = "d".repeat(64);
        let mut first = recovery
            .begin_durable_cas_upload_admitted(
                &guard,
                &owner,
                "external-content-import",
                &key,
                None,
            )
            .unwrap();
        let mut second = recovery
            .begin_durable_cas_upload_admitted(
                &guard,
                &owner,
                "external-content-import",
                &key,
                None,
            )
            .unwrap();
        let first_id = first.staging_id().to_owned();
        let second_id = second.staging_id().to_owned();

        // Identical acquisition request, independent consumer binding targets.
        // Keep the second lock held: settling the first must neither inspect
        // nor wait for another caller's upload.
        settle_current_binding_receipt(&mut first, &guard, &first_binding).unwrap();
        assert_eq!(second.admitted_target_hash(), None);
        settle_current_binding_receipt(&mut second, &guard, &second_binding).unwrap();
        settle_current_binding_receipt(&mut first, &guard, &first_binding).unwrap();
        assert!(settle_current_binding_receipt(&mut first, &guard, &second_binding).is_err());
        drop(first);
        drop(second);
        for (id, target) in [(first_id, first_binding), (second_id, second_binding)] {
            let mut reopened = recovery
                .open_durable_cas_upload_admitted(&guard, &id, &owner)
                .unwrap();
            reopened.ensure_publication_contract(&key, None).unwrap();
            assert_eq!(reopened.admitted_target_hash(), Some(target.as_str()));
            settle_current_binding_receipt(&mut reopened, &guard, &target).unwrap();
        }
    }

    #[test]
    fn binding_request_consumer_coordinates_are_clean_cut() {
        let bundle = BindRequest {
            staging_id: "stage".to_owned(),
            request_digest: "a".repeat(64),
            manifest_hash: "b".repeat(64),
            consumer_ref: "worker:tests/profile".to_owned(),
            consumer_kind: BindConsumerKind::InstalledBundle,
            project_snapshot_hash: None,
            project_path: None,
            product_selections: None,
            product_owner_principal: None,
        };
        assert!(bundle.validate_consumer_request().is_ok());
        let selection =
            ryeos_state::external_content::products::composition::ProductSelection {
                declaration_id: "runtime".to_owned(),
                witness_hash: "f".repeat(64),
                witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                qualification_hash: None,
            };
        let selected_bundle = BindRequest {
            product_selections: Some(vec![selection.clone()]),
            product_owner_principal: Some(format!("fp:{}", "d".repeat(64))),
            ..bundle.clone()
        };
        assert!(selected_bundle.validate_consumer_request().is_ok());
        assert!(matches!(
            selected_bind_subject(&selected_bundle).unwrap(),
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless
        ));
        assert!(
            ensure_unselected_bind_request(&selected_bundle, BindConsumerKind::InstalledBundle)
                .is_err()
        );
        assert!(
            BindRequest {
                product_selections: Some(Vec::new()),
                product_owner_principal: Some(format!("fp:{}", "d".repeat(64))),
                ..bundle.clone()
            }
            .validate_consumer_request()
            .is_err()
        );
        let project = BindRequest {
            consumer_kind: BindConsumerKind::PinnedProject,
            project_snapshot_hash: Some("c".repeat(64)),
            project_path: Some(std::path::PathBuf::from("/target/project")),
            ..bundle.clone()
        };
        assert!(project.validate_consumer_request().is_ok());
        let selected_project = BindRequest {
            product_selections: Some(vec![selection]),
            product_owner_principal: Some(format!("fp:{}", "d".repeat(64))),
            ..project.clone()
        };
        assert!(matches!(
            selected_bind_subject(&selected_project).unwrap(),
            ryeos_engine::contracts::SubjectResolutionAuthority::PinnedGeneration {
                snapshot_hash
            } if snapshot_hash == "c".repeat(64)
        ));
        assert!(
            ensure_unselected_bind_request(&selected_project, BindConsumerKind::PinnedProject)
                .is_err()
        );
        assert!(
            BindRequest {
                project_path: None,
                ..project.clone()
            }
            .validate_consumer_request()
            .is_err()
        );
        assert!(selected_bind_subject(&bundle).is_err());
        assert!(
            BindRequest {
                product_selections: Some(vec![
                    ryeos_state::external_content::products::composition::ProductSelection {
                        declaration_id: "runtime".to_owned(),
                        witness_hash: "f".repeat(64),
                        witness_source: ryeos_state::external_content::products::transfer::ProductWitnessSource::LocalCapture {},
                        qualification_hash: None,
                    }
                ]),
                ..bundle.clone()
            }
            .validate_consumer_request()
            .is_err()
        );
        assert!(
            BindRequest {
                product_owner_principal: Some(format!("fp:{}", "d".repeat(64))),
                ..bundle
            }
            .validate_consumer_request()
            .is_err()
        );
    }

    #[test]
    fn selected_binding_consumer_kind_must_match_exact_resolution_source() {
        let installed = ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
            "tool:fixture/verify".to_owned(),
            "a".repeat(64),
        )
        .unwrap();
        let project = ryeos_state::objects::ExternalContentConsumerAuthority::pinned_project(
            "config:fixture/worker".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            None,
        )
        .unwrap();
        ensure_bind_consumer_kind(Some(BindConsumerKind::InstalledBundle), &installed).unwrap();
        ensure_bind_consumer_kind(Some(BindConsumerKind::PinnedProject), &project).unwrap();
        ensure_bind_consumer_kind(None, &installed).unwrap();
        assert!(
            ensure_bind_consumer_kind(Some(BindConsumerKind::InstalledBundle), &project).is_err()
        );
        assert!(
            ensure_bind_consumer_kind(Some(BindConsumerKind::PinnedProject), &installed).is_err()
        );
    }

    #[test]
    fn import_identity_is_path_free_and_commits_the_open_root_identity() {
        let request = FilesystemImportRequest {
            root: "models".to_owned(),
            path: "qwen".to_owned(),
            shape: ImportShape::Tree,
            storage: ImportStorage::Content,
            maximum_bytes: 4096,
            expected_file_sha256: None,
        };
        let limits = crate::node_policy::sections::external_content::ExternalContentImportLimits {
            max_depth: 8,
            max_entries: 32,
            max_file_bytes: 4096,
            max_total_bytes: 4096,
            store_budget_bytes: 8192,
            minimum_free_bytes: 1024,
        };
        let ignore = crate::ignore::IgnoreMatcher::from_config(&crate::ignore::IgnoreConfig {
            patterns: Vec::new(),
        })
        .unwrap();
        let first = import_request_digest(&request, &limits, 7, 11, &ignore).unwrap();
        let same_open_root = import_request_digest(&request, &limits, 7, 11, &ignore).unwrap();
        let rebound_root = import_request_digest(&request, &limits, 8, 11, &ignore).unwrap();
        assert_eq!(first, same_open_root);
        assert_ne!(first, rebound_root);
    }

    #[test]
    fn source_tree_must_remain_on_the_admitted_root_filesystem() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("tree")).unwrap();
        let root = lillux::PinnedDirectory::open(directory.path())
            .unwrap()
            .unwrap();
        let (device, _) = root.device_inode().unwrap();
        open_admitted_source_tree(&root, "tree", device).unwrap();
        assert!(
            open_admitted_source_tree(&root, "tree", device.saturating_add(1))
                .unwrap_err()
                .to_string()
                .contains("crossed")
        );
    }

    #[test]
    fn import_capacity_reserves_allocation_units_and_file_identities() {
        let identities = 10 * IMPORT_ALLOCATION_UNITS_PER_ENTRY + IMPORT_FIXED_ALLOCATION_UNITS;
        let required = 100 + 1_000 + identities * 4_096;
        require_import_store_capacity(
            "fixture store",
            lillux::FilesystemCapacity {
                total_bytes: required,
                available_bytes: required,
                allocation_unit_bytes: 4_096,
                available_files: identities,
            },
            100,
            1_000,
            10,
        )
        .unwrap();
        for capacity in [
            lillux::FilesystemCapacity {
                total_bytes: required,
                available_bytes: required - 1,
                allocation_unit_bytes: 4_096,
                available_files: identities,
            },
            lillux::FilesystemCapacity {
                total_bytes: required,
                available_bytes: required,
                allocation_unit_bytes: 4_096,
                available_files: identities - 1,
            },
        ] {
            assert!(
                require_import_store_capacity("fixture store", capacity, 100, 1_000, 10).is_err()
            );
        }
    }

    #[test]
    fn retained_binding_projection_is_exact_and_release_is_not_ready() {
        let manifest_hash = "a".repeat(64);
        let publisher = "b".repeat(64);
        let consumer = "worker:tests/profile";
        let consumer_authority =
            ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
                consumer.to_owned(),
                publisher.clone(),
            )
            .unwrap();
        let target_node = "c".repeat(64);
        let active = ryeos_state::objects::ExternalContentBinding::active(
            manifest_hash.clone(),
            ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
            consumer_authority.clone(),
            target_node.clone(),
            "d".repeat(64),
            "e".repeat(64),
        )
        .unwrap();
        assert!(
            binding_authorizes_consumer(
                &active,
                &active.binding_subject_id,
                &manifest_hash,
                &consumer_authority,
                &target_node,
                ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            )
            .unwrap()
        );

        let released = ryeos_state::objects::ExternalContentBinding::released_from(
            &active,
            "f".repeat(64),
            "1".repeat(64),
        )
        .unwrap();
        assert!(
            !binding_authorizes_consumer(
                &released,
                &released.binding_subject_id,
                &manifest_hash,
                &consumer_authority,
                &target_node,
                ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            )
            .unwrap()
        );

        assert!(
            binding_authorizes_consumer(
                &active,
                &active.binding_subject_id,
                &manifest_hash,
                &ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
                    "worker:tests/other".to_owned(),
                    publisher.clone(),
                )
                .unwrap(),
                &target_node,
                ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            )
            .is_err()
        );
        let wrong_publisher = "2".repeat(64);
        assert!(
            binding_authorizes_consumer(
                &active,
                &active.binding_subject_id,
                &manifest_hash,
                &ryeos_state::objects::ExternalContentConsumerAuthority::installed_bundle(
                    consumer.to_owned(),
                    wrong_publisher,
                )
                .unwrap(),
                &target_node,
                ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND,
            )
            .is_err()
        );
    }
}

fn open_directory_relative(
    base: &lillux::PinnedDirectory,
    relative: &str,
) -> anyhow::Result<lillux::PinnedDirectory> {
    let mut current = base.try_clone()?;
    for component in relative.split('/') {
        current = current
            .open_child_directory(OsStr::new(component))?
            .ok_or_else(|| anyhow::anyhow!("external-content source directory is unavailable"))?;
    }
    Ok(current)
}

fn open_file_parent<'a>(
    base: &lillux::PinnedDirectory,
    relative: &'a str,
) -> anyhow::Result<(lillux::PinnedDirectory, &'a str)> {
    let (parent, name) = relative.rsplit_once('/').unwrap_or(("", relative));
    let parent = if parent.is_empty() {
        base.try_clone()?
    } else {
        open_directory_relative(base, parent)?
    };
    Ok((parent, name))
}
