//! Operator-owned external-content import, binding, and integrity operations.
//!
//! The API layer authenticates ordinary signed service execution; this module
//! additionally requires the configured local operator identity, resolves the
//! system/state-only import policy, and orchestrates meaning-blind state
//! primitives. No runtime callback or manifest authority reaches this code.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

use crate::handler_context::HandlerContext;
use crate::state::AppState;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
pub struct ImportRequest {
    pub root: String,
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindResponse {
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
    pub binding_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseResponse {
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
    pub binding_id: String,
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

/// Authenticate the configured local operator principal and return its raw
/// key fingerprint for state objects whose schema stores fingerprints rather
/// than canonical `fp:<digest>` principal IDs.
pub fn require_local_operator(
    state: &AppState,
    context: &HandlerContext,
) -> anyhow::Result<String> {
    if context.authenticated_origin_site_id.is_some() {
        bail!("external-content operator actions are unavailable to remote-origin principals");
    }
    require_configured_operator(state, context)
}

/// Authenticate the node's configured operator principal without constraining
/// transport origin. Hosted execution is deliberately remote-operable: the
/// authenticated origin is retained as evidence, but it cannot replace or
/// weaken the exact configured-operator fingerprint check.
pub fn require_configured_operator(
    state: &AppState,
    context: &HandlerContext,
) -> anyhow::Result<String> {
    let operator = crate::identity::NodeIdentity::load(&state.config.operator_signing_key_path)
        .context("load configured operator identity")?;
    authenticated_configured_operator_fingerprint(context, &operator)
}

fn authenticated_configured_operator_fingerprint(
    context: &HandlerContext,
    operator: &crate::identity::NodeIdentity,
) -> anyhow::Result<String> {
    context
        .require_verified()
        .map_err(|error| anyhow::anyhow!(error))?;
    if context.fingerprint != operator.principal_id() {
        bail!("action requires the configured operator");
    }
    Ok(operator.fingerprint().to_owned())
}

pub async fn import(
    state: Arc<AppState>,
    context: HandlerContext,
    request: ImportRequest,
) -> anyhow::Result<ImportResponse> {
    let operator_fingerprint = require_local_operator(&state, &context)?;
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
    let policy = state
        .node_config
        .external_content_import_policy
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("node has no external-content import policy"))?;
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
    let capacity = large_store.filesystem_capacity()?;
    let required_free = policy
        .limits
        .minimum_free_bytes
        .checked_add(request.maximum_bytes)
        .ok_or_else(|| anyhow::anyhow!("external-content free-space requirement overflow"))?;
    if capacity.available_bytes < required_free {
        bail!(
            "external-content import requires {required_free} available bytes, observed {}",
            capacity.available_bytes
        );
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
            policy,
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
            policy,
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

pub async fn bind(
    state: Arc<AppState>,
    context: HandlerContext,
    request: BindRequest,
) -> anyhow::Result<BindResponse> {
    let operator_fingerprint = require_local_operator(&state, &context)?;
    if !lillux::valid_hash(&request.request_digest) || !lillux::valid_hash(&request.manifest_hash) {
        bail!("external-content bind request contains a non-canonical digest");
    }
    let authority = state.state_store.pinned_state_authority()?;
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
    let consumer = resolve_external_content_consumer(
        &state,
        &request.consumer_ref,
        &request.manifest_hash,
        manifest_kind,
    )?;
    let binding_id = ryeos_state::objects::ExternalContentBinding::derive_binding_id(
        &request.manifest_hash,
        &consumer.consumer_ref,
        &consumer.publisher_fingerprint,
    )?;
    let publication_key =
        ryeos_state::DurableCasPublicationKey::external_content_import(&request.request_digest)?;
    let mut stage = authority
        .require_recovery()?
        .open_durable_cas_upload_admitted(&guard, &request.staging_id, &operator_fingerprint)?;
    stage.ensure_publication_contract(&publication_key, None)?;

    if let Some(binding_hash) = stage.admitted_target_hash() {
        let current = state
            .state_store
            .with_state_db(|db| db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, &binding_id))?;
        if current.as_ref().map(|head| head.target_hash.as_str()) != Some(binding_hash) {
            bail!("admitted external-content binding receipt is not the current signed head");
        }
        return Ok(BindResponse {
            binding_id,
            binding_hash: binding_hash.to_owned(),
            manifest_hash: request.manifest_hash,
            consumer_ref: consumer.consumer_ref,
            publisher_fingerprint: consumer.publisher_fingerprint,
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
                ryeos_state::object_closure::ObjectClosureLimits::default(),
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
            if consumer
                .grant_max_total_bytes
                .is_some_and(|maximum| manifest.total_bytes > maximum)
            {
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
                ryeos_state::object_closure::ObjectClosureLimits::default(),
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
        consumer.consumer_ref.clone(),
        consumer.publisher_fingerprint.clone(),
        operator_fingerprint,
    )?;
    let binding_hash = stage.store_object(&guard, &cas, &binding.to_value()?)?;
    let binding_closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [binding_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::default(),
    )?;
    if !binding_closure.is_complete() {
        bail!("external-content binding closure is incomplete");
    }
    stage.protect_cas_closure(
        &guard,
        binding_closure.object_hashes.iter().map(String::as_str),
        binding_closure.blob_hashes.iter().map(String::as_str),
    )?;
    for hash in &binding_closure.large_object_hashes {
        stage.ensure_protects_large_object(hash)?;
    }
    debug_assert!(closure.object_hashes.contains(&request.manifest_hash));

    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot acquire external-content write permit: {error}")
        })?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
        db.ensure_current_external_content_binding_epoch(&guard)?;
        let current = db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, &binding_id)?;
        if let Some(current) = current.as_ref()
            && current.target_hash == binding_hash
        {
            return Ok(());
        }
        db.advance_generic_head_ref(
            BINDING_HEAD_NAMESPACE,
            &binding_id,
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
        binding_id,
        binding_hash,
        manifest_hash: request.manifest_hash,
        consumer_ref: consumer.consumer_ref,
        publisher_fingerprint: consumer.publisher_fingerprint,
        idempotent: false,
    })
}

pub async fn scrub(state: Arc<AppState>, context: HandlerContext) -> anyhow::Result<ScrubResponse> {
    require_local_operator(&state, &context)?;
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
            if head.namespace != BINDING_HEAD_NAMESPACE || head.name != binding.binding_id {
                bail!("binding head coordinates contradict the retained binding");
            }
            if binding.state == ryeos_state::objects::ExternalContentBindingState::Active {
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
                binding_id: head.name,
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
    let operator_fingerprint = require_local_operator(&state, &context)?;
    if !lillux::valid_hash(&request.binding_id)
        || request
            .binding_id
            .bytes()
            .any(|byte| byte.is_ascii_uppercase())
    {
        bail!("external-content release binding_id is not a canonical digest");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let worker_state = state.clone();
    let binding_id = request.binding_id;
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
                &binding_id,
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
        .quiesce(std::time::Duration::from_secs(30))
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
    binding_id: &str,
    operator_fingerprint: &str,
) -> anyhow::Result<ReleaseResponse> {
    let cas = authority.cas_store()?;
    let current = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, binding_id))?
        .ok_or_else(|| anyhow::anyhow!("external-content binding does not exist"))?;
    let value = cas
        .get_object(&current.target_hash)?
        .ok_or_else(|| anyhow::anyhow!("external-content binding head target is absent"))?;
    let active = ryeos_state::objects::ExternalContentBinding::from_value(&value)?;
    if active.binding_id != binding_id {
        bail!("external-content binding head identity is inconsistent");
    }
    if active.state == ryeos_state::objects::ExternalContentBindingState::Released {
        return Ok(ReleaseResponse {
            binding_id: active.binding_id,
            binding_hash: current.target_hash,
            manifest_hash: active.manifest_hash,
            consumer_ref: active.consumer_ref,
            publisher_fingerprint: active.publisher_fingerprint,
            idempotent: true,
        });
    }
    let released = ryeos_state::objects::ExternalContentBinding::released_from(
        &active,
        operator_fingerprint.to_owned(),
    )?;
    let mut stage = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(guard, "external-content-binding-release")?;
    let released_hash = stage.store_object_admitted(guard, &cas, &released.to_value()?)?;
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
        db.advance_generic_head_ref(
            BINDING_HEAD_NAMESPACE,
            binding_id,
            &released_hash,
            Some(&current.target_hash),
            &signer,
            guard,
        )
    })?;
    if let Err(error) = stage.finish_admitted(guard) {
        tracing::warn!(%error, %binding_id, "released binding head published while temporary root remained recoverable");
    }
    Ok(ReleaseResponse {
        binding_id: released.binding_id,
        binding_hash: released_hash,
        manifest_hash: released.manifest_hash,
        consumer_ref: released.consumer_ref,
        publisher_fingerprint: released.publisher_fingerprint,
        idempotent: false,
    })
}

struct ResumeWriteBarrier(Arc<crate::write_barrier::WriteBarrier>);

impl Drop for ResumeWriteBarrier {
    fn drop(&mut self) {
        self.0.resume();
    }
}

pub fn require_active_binding(
    state: &AppState,
    cas: &lillux::CasStore,
    manifest_hash: &str,
    consumer_ref: &str,
    publisher_fingerprint: &str,
) -> anyhow::Result<ryeos_state::objects::ExternalContentBinding> {
    let binding_id = ryeos_state::objects::ExternalContentBinding::derive_binding_id(
        manifest_hash,
        consumer_ref,
        publisher_fingerprint,
    )?;
    let head = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref(BINDING_HEAD_NAMESPACE, &binding_id))?
        .ok_or_else(|| anyhow::anyhow!("external-content consumer has no operator binding"))?;
    let value = cas
        .get_object(&head.target_hash)?
        .ok_or_else(|| anyhow::anyhow!("external-content binding head target is absent"))?;
    let binding = ryeos_state::objects::ExternalContentBinding::from_value(&value)?;
    let manifest_kind = cas
        .get_object(manifest_hash)?
        .and_then(|value| {
            value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| anyhow::anyhow!("external-content binding manifest is absent or untyped"))?;
    if binding.state != ryeos_state::objects::ExternalContentBindingState::Active
        || binding.binding_id != binding_id
        || binding.manifest_hash != manifest_hash
        || binding.consumer_ref != consumer_ref
        || binding.publisher_fingerprint != publisher_fingerprint
        || binding.manifest_kind != manifest_kind
    {
        bail!("external-content binding does not authorize this consumer");
    }
    Ok(binding)
}

struct ResolvedConsumer {
    consumer_ref: String,
    publisher_fingerprint: String,
    declaration_kind: ryeos_engine::external_content::ExternalContentKind,
    grant_max_total_bytes: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
fn capture_content_import(
    request: &ImportRequest,
    policy: &crate::node_config::sections::external_content::ExternalContentImportPolicyRecord,
    source_root: &lillux::PinnedDirectory,
    root_device: u64,
    configured_ignore: &ryeos_state::ignore::IgnoreMatcher,
    guard: &ryeos_state::CasMutationGuard,
    cas: &lillux::CasStore,
    stage: &mut ryeos_state::DurableCasUploadStage,
    request_digest: String,
) -> anyhow::Result<ImportResponse> {
    let mut budget = ryeos_state::LaunchCaptureBudget::bounded(
        policy.limits.max_depth.min(ryeos_state::MAX_CAPTURE_DEPTH),
        policy
            .limits
            .max_entries
            .min(ryeos_state::MAX_CAPTURE_ENTRIES),
        policy
            .limits
            .max_file_bytes
            .min(request.maximum_bytes)
            .min(ryeos_state::MAX_CAPTURE_FILE_BYTES),
        request.maximum_bytes.min(ryeos_state::MAX_CAPTURE_BYTES),
    )?;
    let capture_policy =
        ryeos_state::ExternalCapturePolicy::new(request.path.clone(), configured_ignore)?;
    let mut sink = DurableContentSink { guard, cas, stage };
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
    let manifest_hash = sink
        .stage
        .store_object(guard, cas, &serde_json::to_value(&manifest)?)?;
    let verified = ryeos_state::VerifiedExternalContentClosure::load(cas, &manifest_hash)?;
    if verified.manifest() != &manifest {
        bail!("stored content manifest differs from captured value");
    }
    Ok(ImportResponse {
        staging_id: sink.stage.staging_id().to_owned(),
        request_digest,
        manifest_hash,
        manifest_kind: ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND.to_owned(),
        entry_count: manifest.entry_count,
        total_bytes: manifest.total_bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn capture_large_import(
    request: &ImportRequest,
    policy: &crate::node_config::sections::external_content::ExternalContentImportPolicyRecord,
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
        max_depth: policy.limits.max_depth,
        max_entries: policy
            .limits
            .max_entries
            .min(ryeos_state::objects::MAX_LARGE_CONTENT_MANIFEST_ENTRIES),
        max_file_bytes: policy.limits.max_file_bytes.min(request.maximum_bytes),
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
    let manifest_hash = sink.stage.store_object(guard, cas, &manifest.to_value()?)?;
    let loaded = ryeos_state::objects::load_if_large_content_manifest(cas, &manifest_hash)?
        .ok_or_else(|| anyhow::anyhow!("stored large-content manifest changed kind"))?;
    if loaded != manifest {
        bail!("stored large-content manifest differs from captured value");
    }
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
    guard: &'a ryeos_state::CasMutationGuard,
    cas: &'a lillux::CasStore,
    stage: &'a mut ryeos_state::DurableCasUploadStage,
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
        self.stage.protect_cas_closure(
            self.guard,
            std::iter::empty(),
            std::iter::once(outcome.hash.as_str()),
        )?;
        Ok((outcome.hash, outcome.size))
    }
}

fn resolve_external_content_consumer(
    state: &AppState,
    requested_ref: &str,
    manifest_hash: &str,
    manifest_kind: &str,
) -> anyhow::Result<ResolvedConsumer> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(requested_ref)
        .map_err(|error| anyhow::anyhow!("invalid consumer ref: {error}"))?;
    if canonical.to_string() != requested_ref {
        bail!("external-content consumer ref must be canonical");
    }
    let effective = state.engine.with_checked_bundle_generation(|generation| {
        generation.effective_item(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: canonical,
            expected_kind: None,
            project_root: None,
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        })
    })?;
    if !effective.trusted
        || effective.trust_class != ryeos_engine::resolution::TrustClass::TrustedBundle
        || effective.source.bundle_root.is_none()
    {
        bail!("external-content consumer must be a trusted installed-bundle item");
    }
    let publisher_fingerprint = effective
        .provenance
        .root
        .signer_fingerprint
        .clone()
        .ok_or_else(|| anyhow::anyhow!("trusted external-content consumer has no signer"))?;
    let external_contract = state
        .engine
        .kinds
        .get(&effective.kind)
        .and_then(|kind| kind.execution.as_ref())
        .and_then(|execution| execution.external_content.as_ref())
        .ok_or_else(|| anyhow::anyhow!("consumer kind has no signed external-content contract"))?;
    let grant_max_total_bytes = match manifest_kind {
        ryeos_state::objects::EXTERNAL_CONTENT_MANIFEST_KIND => None,
        ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND => Some(
            external_contract
                .large_content
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("consumer kind has no signed large-content grant"))?
                .max_total_bytes
                .unwrap_or(ryeos_state::objects::MAX_LARGE_CONTENT_TOTAL_BYTES),
        ),
        other => bail!("unsupported external-content manifest kind `{other}`"),
    };
    let declarations = effective
        .composed_value
        .get("external_content")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("consumer does not declare external content"))?;
    let declarations: Vec<ryeos_engine::external_content::ExternalContentDeclaration> =
        serde_json::from_value(declarations)?;
    let declaration = declarations.iter().find(|declaration| {
        declaration.mode == ryeos_engine::external_content::ExternalContentMode::Pinned
            && declaration.digest.as_deref() == Some(manifest_hash)
            && declaration.locator.is_none()
    });
    let declaration = declaration
        .ok_or_else(|| anyhow::anyhow!("consumer does not declare the staged manifest digest"))?;
    Ok(ResolvedConsumer {
        consumer_ref: effective.canonical_ref,
        publisher_fingerprint,
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
        let hash = self.stage.store_blob(self.guard, self.cas, &bytes)?;
        Ok((hash, expected_size))
    }
}

fn import_request_digest(
    request: &ImportRequest,
    limits: &crate::node_config::sections::external_content::ExternalContentImportLimits,
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
    fn import_identity_is_path_free_and_commits_the_open_root_identity() {
        let request = ImportRequest {
            root: "models".to_owned(),
            path: "qwen".to_owned(),
            shape: ImportShape::Tree,
            storage: ImportStorage::Content,
            maximum_bytes: 4096,
            expected_file_sha256: None,
        };
        let limits = crate::node_config::sections::external_content::ExternalContentImportLimits {
            max_depth: 8,
            max_entries: 32,
            max_file_bytes: 4096,
            max_total_bytes: 4096,
            store_budget_bytes: 8192,
            minimum_free_bytes: 1024,
        };
        let ignore = crate::ignore::matcher_from_builtins();
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
    fn configured_operator_auth_uses_principal_id_and_allows_remote_transport() {
        let directory = tempfile::tempdir().unwrap();
        let identity =
            crate::identity::NodeIdentity::create(&directory.path().join("operator.pem")).unwrap();
        let local = HandlerContext::new(identity.principal_id(), vec!["*".to_owned()], true);
        assert_eq!(
            authenticated_configured_operator_fingerprint(&local, &identity).unwrap(),
            identity.fingerprint()
        );

        let raw = HandlerContext::new(identity.fingerprint().to_owned(), vec![], true);
        assert!(authenticated_configured_operator_fingerprint(&raw, &identity).is_err());
        let remote = HandlerContext::new_with_origin(
            identity.principal_id(),
            vec!["*".to_owned()],
            true,
            Some("site:remote".to_owned()),
        );
        assert_eq!(
            authenticated_configured_operator_fingerprint(&remote, &identity).unwrap(),
            identity.fingerprint()
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
