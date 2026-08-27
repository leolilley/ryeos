//! Configured-operator activation of trusted external-content acquisition recipes.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use ryeos_app::managed_external_content::{
    ManagedActivationSource, ManagedMemberDisposition, ResolvedManagedExternalContentActivation,
};
use ryeos_app::managed_external_content_operation::{
    AcquisitionMode, MANAGED_ACTIVATION_OPERATION, ManagedActivationJobOperation,
};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

const CACHE_ENTRY_LIMIT: usize = 4096;
const ERROR_LIMIT: usize = 2048;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub activation_ref: String,
    pub mode: AcquisitionMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub job_id: String,
    pub activation_id: String,
    pub receipt_hash: String,
    pub consumer_ref: String,
    pub state: String,
    pub idempotent: bool,
}

struct ActivationDirectories {
    cache: lillux::PinnedDirectory,
    staging: lillux::PinnedDirectory,
    job: lillux::PinnedDirectory,
    job_name: String,
    _lock: lillux::PinnedDirectoryLock,
}

#[derive(Debug)]
struct ImportedComponent {
    component_id: String,
    import: ryeos_app::operator_external_content::ImportResponse,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let operator = ryeos_app::operator_external_content::require_configured_operator(&state, &ctx)?;
    let activation =
        ryeos_app::managed_external_content::resolve_activation(&state, &req.activation_ref)?;
    let policy = managed_policy(&state)?;
    if req.mode == AcquisitionMode::Online && !policy.allow_online {
        bail!("node policy does not permit online managed external-content acquisition");
    }
    let operator_authority_digest =
        ryeos_app::operator_external_content::configured_operator_authority_digest(
            &state, &operator,
        )?;
    let operation = ManagedActivationJobOperation::new(
        &activation,
        operator,
        operator_authority_digest,
        external_content_policy(&state)?,
        req.mode,
    )?;
    Ok(serde_json::to_value(
        execute_operation(state, activation, operation, true).await?,
    )?)
}

async fn execute_operation(
    state: Arc<AppState>,
    activation: ResolvedManagedExternalContentActivation,
    operation: ManagedActivationJobOperation,
    create_if_absent: bool,
) -> Result<Response> {
    let operation_digest = ryeos_state::objects::canonical_value_digest(&operation.to_value()?)?;
    let job_id = activation_job_id(&operation_digest);
    let directories = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || open_activation_directories(&state, &operation_digest)
    })
    .await
    .context("managed activation directory task panicked")??;

    let existing = state.state_store.with_state_db(|db| {
        if let Some(job) = db.get_sync_job(&job_id)? {
            return Ok(job);
        }
        if !create_if_absent {
            bail!("managed activation recovery job disappeared");
        }
        db.create_sync_job(&ryeos_state::NewSyncJob {
            job_id: job_id.clone(),
            operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
            operation: operation.to_value()?,
            peer: None,
            roots: Vec::new(),
            heads: Vec::new(),
            max_attempts: managed_policy(&state)?.max_attempts,
        })
    })?;
    if existing.operation != operation.to_value()? {
        bail!("managed activation job id is retained for another canonical operation");
    }
    if existing.state == ryeos_state::SyncJobState::Completed {
        let mut response: Response = serde_json::from_value(
            existing
                .result
                .ok_or_else(|| anyhow::anyhow!("completed activation job has no result"))?,
        )?;
        validate_completed_activation(&state, &activation, &operation, &job_id, &response)?;
        response.idempotent = true;
        if let Err(error) = tokio::task::spawn_blocking(move || cleanup_staging(directories))
            .await
            .context("completed activation cleanup task panicked")?
        {
            tracing::warn!(%error, %job_id, "idempotent managed activation retained rebuildable staging");
        }
        return Ok(response);
    }
    if matches!(
        existing.state,
        ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled
    ) {
        let state_name = existing.state.as_str().to_owned();
        let diagnostic = existing
            .last_error
            .clone()
            .unwrap_or_else(|| "no retained diagnostic".to_owned());
        cleanup_terminal_staging(directories, &job_id).await;
        bail!(
            "managed activation job {} is terminal in state {}: {}",
            job_id,
            state_name,
            diagnostic
        );
    }
    if let Err(error) = (|| {
        let operator_authority_digest =
            ryeos_app::operator_external_content::configured_operator_authority_digest(
                &state,
                &operation.operator_fingerprint,
            )?;
        operation.validate_current(
            &activation,
            external_content_policy(&state)?,
            &operator_authority_digest,
        )
    })() {
        let detail = bounded_error(&format!(
            "retained managed activation no longer matches current signed or node authority: {error:#}"
        ));
        state.state_store.with_state_db(|db| {
            db.update_sync_job(
                &job_id,
                &ryeos_state::SyncJobUpdate {
                    state: ryeos_state::SyncJobState::Failed,
                    phase: "authority_changed".to_owned(),
                    roots: None,
                    heads: None,
                    uploaded_hashes: existing.uploaded_hashes.clone(),
                    fetched_hashes: existing.fetched_hashes.clone(),
                    last_error: Some(detail),
                    result: existing.result.clone(),
                },
            )
        })?;
        cleanup_terminal_staging(directories, &job_id).await;
        return Err(error);
    }

    let attempt_id = format!(
        "external-content-activation-attempt:{}",
        uuid::Uuid::new_v4()
    );
    if let Err(error) = state.state_store.with_state_db(|db| {
        db.create_sync_job_attempt(&ryeos_state::NewSyncJobAttempt {
            attempt_id: attempt_id.clone(),
            job_id: job_id.clone(),
            worker_id: Some("managed-external-content".to_owned()),
            phase: "acquiring".to_owned(),
        })?;
        Ok(())
    }) {
        let latest = state
            .state_store
            .with_state_db(|db| db.get_sync_job(&job_id))?
            .context("managed activation job disappeared")?;
        let mut terminalized = false;
        if latest.attempt_count >= latest.max_attempts
            && latest.state == ryeos_state::SyncJobState::Retryable
        {
            state.state_store.with_state_db(|db| {
                db.update_sync_job(
                    &job_id,
                    &ryeos_state::SyncJobUpdate {
                        state: ryeos_state::SyncJobState::Failed,
                        phase: "attempts_exhausted".to_owned(),
                        roots: None,
                        heads: None,
                        uploaded_hashes: latest.uploaded_hashes,
                        fetched_hashes: latest.fetched_hashes,
                        last_error: Some(
                            "managed activation exhausted its admitted attempts".to_owned(),
                        ),
                        result: latest.result,
                    },
                )
            })?;
            terminalized = true;
        }
        if terminalized {
            cleanup_terminal_staging(directories, &job_id).await;
        }
        return Err(error);
    }

    let run = run_attempt(Arc::clone(&state), &activation, &operation, &directories).await;
    match run {
        Ok(publication) => {
            let response = Response {
                job_id: job_id.clone(),
                activation_id: publication.activation_id,
                receipt_hash: publication.receipt_hash,
                consumer_ref: activation.document.consumer_ref,
                state: "completed".to_owned(),
                idempotent: publication.idempotent,
            };
            let result = serde_json::to_value(&response)?;
            settle_attempt(
                &state,
                &job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Completed,
                ryeos_state::SyncJobState::Completed,
                "completed",
                Some(response.receipt_hash.clone()),
                None,
                Some(result),
            )?;
            if let Err(error) = tokio::task::spawn_blocking(move || cleanup_staging(directories))
                .await
                .context("managed activation cleanup task panicked")?
            {
                tracing::warn!(%error, %job_id, "completed managed activation retained rebuildable staging");
            }
            Ok(response)
        }
        Err(error) => {
            let detail = bounded_error(&format!("{error:#}"));
            let latest = state
                .state_store
                .with_state_db(|db| db.get_sync_job(&job_id))?
                .context("managed activation job disappeared before failure settlement")?;
            let terminal = latest.attempt_count >= latest.max_attempts;
            settle_attempt(
                &state,
                &job_id,
                &attempt_id,
                ryeos_state::SyncJobAttemptState::Failed,
                if terminal {
                    ryeos_state::SyncJobState::Failed
                } else {
                    ryeos_state::SyncJobState::Retryable
                },
                if terminal { "failed" } else { "retryable" },
                None,
                Some(detail),
                latest.result,
            )?;
            if terminal {
                cleanup_terminal_staging(directories, &job_id).await;
            }
            Err(error)
        }
    }
}

fn activation_job_id(operation_digest: &str) -> String {
    format!("external-activation:{operation_digest}")
}

fn validate_completed_activation(
    state: &AppState,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    job_id: &str,
    response: &Response,
) -> Result<()> {
    if response.job_id != job_id
        || response.activation_id != operation.activation_id
        || response.consumer_ref != activation.document.consumer_ref
        || response.state != "completed"
        || !lillux::valid_hash(&response.receipt_hash)
    {
        bail!("completed managed activation result contradicts its durable operation");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let _permit = state
        .write_barrier
        .acquire_with_timeout(ryeos_app::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| {
            anyhow::anyhow!("cannot verify completed activation under write barrier: {error}")
        })?;
    let cas = authority.cas_store()?;
    let head = state
        .state_store
        .with_state_db(|db| {
            db.read_generic_head_ref(
                ryeos_state::objects::EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE,
                &operation.activation_id,
            )
        })?
        .ok_or_else(|| anyhow::anyhow!("completed managed activation head is absent"))?;
    if head.target_hash != response.receipt_hash {
        bail!("completed managed activation result is not the current receipt head");
    }
    let receipt_value = cas
        .get_object(&head.target_hash)?
        .ok_or_else(|| anyhow::anyhow!("completed managed activation receipt is absent"))?;
    let receipt =
        ryeos_state::objects::ExternalContentActivationReceipt::from_value(&receipt_value)?;
    if receipt.activation_id != operation.activation_id
        || receipt.activation_ref != activation.activation_ref
        || receipt.activation_program_digest != activation.activation_program_digest
        || receipt.consumer_ref != activation.document.consumer_ref
        || receipt.publisher_fingerprint != activation.publisher_fingerprint
        || receipt.node_fingerprint != state.identity.fingerprint()
    {
        bail!("completed managed activation receipt contradicts current exact authority");
    }
    let receipt_components = receipt
        .components
        .iter()
        .map(|component| component.id.as_str())
        .collect::<BTreeSet<_>>();
    let expected_components = activation
        .components
        .iter()
        .map(|component| component.recipe.id.as_str())
        .collect::<BTreeSet<_>>();
    if receipt_components != expected_components {
        bail!("completed managed activation receipt has a different component set");
    }
    for component in &activation.components {
        let binding = ryeos_app::operator_external_content::require_active_binding(
            state,
            &cas,
            &component.expected_manifest_hash,
            &activation.document.consumer_ref,
            &activation.publisher_fingerprint,
        )
        .with_context(|| {
            format!(
                "completed managed activation component `{}` is no longer active",
                component.recipe.id
            )
        })?;
        let receipt_component = receipt
            .components
            .iter()
            .find(|candidate| candidate.id == component.recipe.id)
            .expect("component set equality checked above");
        let binding_hash = ryeos_state::objects::canonical_value_digest(&binding.to_value()?)?;
        if binding_hash != receipt_component.binding_hash
            || binding.manifest_kind != component.expected_manifest_kind
        {
            bail!(
                "completed managed activation component `{}` contradicts its receipt or consumer storage grant",
                component.recipe.id
            );
        }
    }
    authority.ensure_guard(&guard)?;
    Ok(())
}

async fn run_attempt(
    state: Arc<AppState>,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    directories: &ActivationDirectories,
) -> Result<ryeos_app::managed_external_content_operation::ManagedActivationPublication> {
    let imported = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let activation = activation.clone();
        let operation = operation.clone();
        let cache = directories.cache.try_clone()?;
        let job = directories.job.try_clone()?;
        move || acquire_and_import(&state, &activation, &operation, &cache, &job)
    })
    .await
    .context("managed acquisition task panicked")??;

    let mut receipts = Vec::with_capacity(imported.len());
    for imported in imported {
        let binding = ryeos_app::operator_external_content::bind_managed_activation_component(
            Arc::clone(&state),
            operation.operator_fingerprint.clone(),
            activation,
            ryeos_app::operator_external_content::BindRequest {
                staging_id: imported.import.staging_id,
                request_digest: imported.import.request_digest,
                manifest_hash: imported.import.manifest_hash,
                consumer_ref: activation.document.consumer_ref.clone(),
            },
        )
        .await?;
        receipts.push(
            ryeos_state::objects::ExternalContentActivationComponentReceipt {
                id: imported.component_id,
                binding_hash: binding.binding_hash,
            },
        );
    }
    tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        let activation = activation.clone();
        let operation = operation.clone();
        move || {
            ryeos_app::managed_external_content_operation::publish_activation_receipt(
                &state,
                &activation,
                &operation,
                receipts,
            )
        }
    })
    .await
    .context("managed activation publication task panicked")?
}

fn acquire_and_import(
    state: &AppState,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    cache: &lillux::PinnedDirectory,
    staging: &lillux::PinnedDirectory,
) -> Result<Vec<ImportedComponent>> {
    let operator_authority_digest =
        ryeos_app::operator_external_content::configured_operator_authority_digest(
            state,
            &operation.operator_fingerprint,
        )?;
    operation.validate_current(
        activation,
        external_content_policy(state)?,
        &operator_authority_digest,
    )?;
    for source in &activation.document.sources {
        let archive = obtain_archive(
            cache,
            source,
            operation.acquisition_mode,
            managed_policy(state)?,
        )?;
        extract_selected_members(archive, source, activation, staging, managed_policy(state)?)?;
    }
    let mut imported = Vec::with_capacity(activation.components.len());
    for component in &activation.components {
        let response = ryeos_app::operator_external_content::import_managed_activation_component(
            state,
            &operation.operator_fingerprint,
            activation,
            component,
            staging,
            &component.recipe.id,
        )?;
        imported.push(ImportedComponent {
            component_id: component.recipe.id.clone(),
            import: response,
        });
    }
    Ok(imported)
}

fn open_activation_directories(
    state: &AppState,
    operation_digest: &str,
) -> Result<ActivationDirectories> {
    if !lillux::valid_hash(operation_digest) {
        bail!("managed activation operation digest is not canonical");
    }
    let state_root = lillux::PinnedDirectory::open(&state.config.runtime_state_dir())?
        .ok_or_else(|| anyhow::anyhow!("runtime state root is unavailable"))?;
    let cache_root = state_root.open_or_create_child(OsStr::new("cache"), 0o700)?;
    let managed_cache =
        cache_root.open_or_create_child(OsStr::new("managed-external-content"), 0o700)?;
    let cache = managed_cache.open_or_create_child(OsStr::new("archives"), 0o700)?;
    let managed = state_root.open_or_create_child(OsStr::new("managed-external-content"), 0o700)?;
    let staging = managed.open_or_create_child(OsStr::new("staging"), 0o700)?;
    let runtime_root = state.config.runtime_root();
    if cache.path() != runtime_root.managed_external_content_cache()
        || staging.path() != runtime_root.managed_external_content_staging()
    {
        bail!("managed external-content layout differs from the typed runtime-root contract");
    }
    let lock = staging.lock_exclusive()?;
    let job_name = operation_digest.to_owned();
    let job = staging.open_or_create_child(OsStr::new(&job_name), 0o700)?;
    lock.ensure_protects(&staging)?;
    Ok(ActivationDirectories {
        cache,
        staging,
        job,
        job_name,
        _lock: lock,
    })
}

fn cleanup_staging(directories: ActivationDirectories) -> Result<()> {
    directories
        .job
        .remove_contents_recursive_bounded(lillux::DirectoryTraversalBudget {
            max_depth: ryeos_state::external_content::MAX_CAPTURE_DEPTH,
            max_entries:
                ryeos_app::managed_external_content::MAX_MANAGED_ACTIVATION_STAGING_ENTRIES,
        })?;
    if !directories
        .staging
        .remove_empty_child_if_same(OsStr::new(&directories.job_name), &directories.job)?
    {
        bail!("managed activation staging remained non-empty after bounded cleanup");
    }
    Ok(())
}

async fn cleanup_terminal_staging(directories: ActivationDirectories, job_id: &str) {
    match tokio::task::spawn_blocking(move || cleanup_staging(directories)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, %job_id, "terminal managed activation retained bounded staging")
        }
        Err(error) => {
            tracing::warn!(%error, %job_id, "terminal managed activation cleanup task panicked")
        }
    }
}

fn obtain_archive(
    cache: &lillux::PinnedDirectory,
    source: &ManagedActivationSource,
    mode: AcquisitionMode,
    policy: &ryeos_app::node_config::sections::external_content::ManagedExternalContentActivationPolicy,
) -> Result<std::fs::File> {
    let name = OsStr::new(&source.sha256);
    if let Some(mut existing) = cache.open_regular(name, false)? {
        let verified = verify_open_file(
            &mut existing,
            source.maximum_compressed_bytes,
            &source.sha256,
            "cached managed activation archive",
        );
        match verified {
            Ok(_) => {
                cache.ensure_path_binding()?;
                return Ok(existing);
            }
            Err(error) => {
                cache
                    .remove_if_same(name, &existing)
                    .context("remove invalid managed activation cache entry")?;
                if mode == AcquisitionMode::Offline {
                    return Err(error).context(
                        "offline managed activation cache entry failed exact verification",
                    );
                }
            }
        }
    }
    if mode == AcquisitionMode::Offline {
        bail!(
            "offline managed activation is missing archive {}",
            source.sha256
        );
    }
    if !policy.allow_online {
        bail!("node policy does not permit online managed activation");
    }
    let retained = cache
        .regular_files_bounded(CACHE_ENTRY_LIMIT)?
        .into_iter()
        .try_fold(0u64, |total, entry| {
            total
                .checked_add(entry.file.metadata()?.len())
                .ok_or_else(|| anyhow::anyhow!("managed archive cache byte count overflow"))
        })?;
    if retained
        .checked_add(source.maximum_compressed_bytes)
        .ok_or_else(|| anyhow::anyhow!("managed archive cache budget overflow"))?
        > policy.cache_budget_bytes
    {
        bail!("managed archive acquisition would exceed the node cache budget");
    }
    let required_free = policy
        .minimum_free_bytes
        .checked_add(source.maximum_compressed_bytes)
        .ok_or_else(|| anyhow::anyhow!("managed archive free-space requirement overflow"))?;
    if cache.filesystem_capacity()?.available_bytes < required_free {
        bail!("managed archive acquisition has insufficient node-private free space");
    }

    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .context("build managed archive HTTPS client")?;
    let mut response = client
        .get(&source.url)
        .header(
            reqwest::header::USER_AGENT,
            "RyeOS-managed-external-content/1",
        )
        .send()
        .context("download managed activation archive")?
        .error_for_status()
        .context("managed activation archive server refused the request")?;
    if response
        .content_length()
        .is_some_and(|length| length > source.maximum_compressed_bytes)
    {
        bail!("managed activation archive content length exceeds its signed bound");
    }
    let created = cache.atomic_create_regular_from_reader(
        name,
        &mut response,
        source.maximum_compressed_bytes,
        0o600,
    )?;
    let mut archive = match created {
        Some((archive, _)) => archive,
        None => cache
            .open_regular(name, false)?
            .ok_or_else(|| anyhow::anyhow!("managed archive publication winner disappeared"))?,
    };
    verify_open_file(
        &mut archive,
        source.maximum_compressed_bytes,
        &source.sha256,
        "downloaded managed activation archive",
    )?;
    cache.ensure_path_binding()?;
    Ok(archive)
}

fn extract_selected_members(
    mut archive_file: std::fs::File,
    source: &ManagedActivationSource,
    activation: &ResolvedManagedExternalContentActivation,
    staging: &lillux::PinnedDirectory,
    policy: &ryeos_app::node_config::sections::external_content::ManagedExternalContentActivationPolicy,
) -> Result<()> {
    archive_file.seek(SeekFrom::Start(0))?;
    let decoder = flate2::read::GzDecoder::new(archive_file);
    let bounded = decoder.take(source.maximum_expanded_bytes.saturating_add(1));
    let mut archive = tar::Archive::new(bounded);
    let selected = source
        .members
        .iter()
        .map(|member| (member.path.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    let imported = activation
        .components
        .iter()
        .flat_map(|component| {
            component
                .recipe
                .members
                .iter()
                .filter(|mapping| mapping.source == source.id)
                .map(move |mapping| (mapping.member.as_str(), (component, mapping)))
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen_paths = BTreeSet::new();
    let mut seen_selected = BTreeSet::new();
    let mut entries = 0usize;
    let mut regular_bytes = 0u64;
    for entry in archive
        .entries()
        .context("read managed activation tar archive")?
    {
        let mut entry = entry.context("read managed activation tar member")?;
        entries = entries
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("managed archive entry count overflow"))?;
        if entries > policy.max_members {
            bail!("managed activation archive exceeds the node entry-count bound");
        }
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            bail!("managed activation archive contains a link or special entry");
        }
        let path_bytes = entry.path_bytes();
        let raw = std::str::from_utf8(path_bytes.as_ref())
            .context("managed activation archive contains a non-UTF8 path")?;
        let path = if entry_type.is_dir() {
            raw.strip_suffix('/').unwrap_or(raw)
        } else {
            raw
        };
        ryeos_state::objects::validate_canonical_project_relative_path(path)
            .context("managed activation archive path is not canonical")?;
        if !seen_paths.insert(path.to_owned()) {
            bail!("managed activation archive repeats a canonical path");
        }
        if entry_type.is_dir() {
            continue;
        }
        let size = entry.header().size()?;
        regular_bytes = regular_bytes
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("managed archive expanded byte count overflow"))?;
        if regular_bytes > source.maximum_expanded_bytes {
            bail!("managed activation archive exceeds its expanded-byte bound");
        }
        let Some(member) = selected.get(path).copied() else {
            continue;
        };
        if size > member.maximum_bytes {
            bail!("selected managed activation member exceeds its signed bound");
        }
        let executable = entry.header().mode()? & 0o111 != 0;
        if executable != member.executable {
            bail!("selected managed activation member executable mode changed");
        }
        seen_selected.insert(path.to_owned());
        if member.disposition == ManagedMemberDisposition::VerifyOnly {
            verify_reader(
                &mut entry,
                member.maximum_bytes,
                &member.sha256,
                "managed activation verification member",
            )?;
            continue;
        }
        let (component, mapping) = imported.get(path).copied().ok_or_else(|| {
            anyhow::anyhow!("imported managed activation member has no resolved consumer component")
        })?;
        let mode = if member.executable { 0o755 } else { 0o644 };
        let (destination, name) = match component.declaration_kind {
            ryeos_engine::external_content::ExternalContentKind::File => {
                if mapping.target.is_some() {
                    bail!("managed file component member unexpectedly has a target");
                }
                (staging.try_clone()?, component.recipe.id.as_str())
            }
            ryeos_engine::external_content::ExternalContentKind::Tree => {
                let target = mapping.target.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("managed tree component member has no target")
                })?;
                let parts = target.split('/').collect::<Vec<_>>();
                let name = parts
                    .last()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("managed tree target is empty"))?;
                let mut destination =
                    staging.open_or_create_child(OsStr::new(&component.recipe.id), 0o755)?;
                for part in parts.iter().take(parts.len().saturating_sub(1)) {
                    destination = destination.open_or_create_child(OsStr::new(part), 0o755)?;
                }
                (destination, name)
            }
        };
        let created = destination.atomic_create_regular_from_reader(
            OsStr::new(name),
            &mut entry,
            member.maximum_bytes,
            mode,
        )?;
        let mut file = match created {
            Some((file, _)) => file,
            None => destination
                .open_regular(OsStr::new(name), false)?
                .ok_or_else(|| anyhow::anyhow!("managed activation staged member disappeared"))?,
        };
        verify_open_file(
            &mut file,
            member.maximum_bytes,
            &member.sha256,
            "managed activation staged member",
        )?;
        if lillux::normalized_portable_regular_mode(&file.metadata()?)? != mode {
            bail!("managed activation staged member mode changed");
        }
        destination.ensure_path_binding()?;
    }
    let mut bounded = archive.into_inner();
    std::io::copy(&mut bounded, &mut std::io::sink())
        .context("finish validating managed activation compressed stream")?;
    let expanded = source
        .maximum_expanded_bytes
        .saturating_add(1)
        .saturating_sub(bounded.limit());
    if expanded > source.maximum_expanded_bytes {
        bail!("managed activation archive exceeds its decompression bound");
    }
    let expected = selected
        .keys()
        .map(|path| (*path).to_owned())
        .collect::<BTreeSet<_>>();
    if seen_selected != expected {
        bail!("managed activation archive is missing a selected member");
    }
    staging.ensure_path_binding()?;
    Ok(())
}

fn verify_open_file(
    file: &mut std::fs::File,
    maximum_bytes: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<u64> {
    file.seek(SeekFrom::Start(0))?;
    let size = verify_reader(file, maximum_bytes, expected_sha256, label)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(size)
}

fn verify_reader(
    reader: &mut impl std::io::Read,
    maximum_bytes: u64,
    expected_sha256: &str,
    label: &str,
) -> Result<u64> {
    let mut digest = Sha256::new();
    let mut observed = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(u64::try_from(read)?)
            .ok_or_else(|| anyhow::anyhow!("{label} byte count overflow"))?;
        if observed > maximum_bytes {
            bail!("{label} exceeds its signed byte bound");
        }
        digest.update(&buffer[..read]);
    }
    let actual = format!("{:x}", digest.finalize());
    if actual != expected_sha256 {
        bail!("{label} digest changed: expected {expected_sha256}, observed {actual}");
    }
    Ok(observed)
}

fn settle_attempt(
    state: &AppState,
    job_id: &str,
    attempt_id: &str,
    attempt_state: ryeos_state::SyncJobAttemptState,
    job_state: ryeos_state::SyncJobState,
    phase: &str,
    receipt_hash: Option<String>,
    error: Option<String>,
    result: Option<Value>,
) -> Result<()> {
    state.state_store.with_state_db(|db| {
        let latest = db
            .get_sync_job(job_id)?
            .ok_or_else(|| anyhow::anyhow!("managed activation job disappeared"))?;
        db.finish_sync_job_attempt_and_update_job(
            attempt_id,
            &ryeos_state::FinishSyncJobAttempt {
                state: attempt_state,
                phase: phase.to_owned(),
                error: error.clone(),
                result: result.clone(),
            },
            job_id,
            &ryeos_state::SyncJobUpdate {
                state: job_state,
                phase: phase.to_owned(),
                roots: receipt_hash.map(|hash| vec![hash]),
                heads: None,
                uploaded_hashes: latest.uploaded_hashes,
                fetched_hashes: latest.fetched_hashes,
                last_error: error,
                result,
            },
        )
    })
}

fn managed_policy(
    state: &AppState,
) -> Result<
    &ryeos_app::node_config::sections::external_content::ManagedExternalContentActivationPolicy,
> {
    state
        .node_config
        .external_content_import_policy
        .as_ref()
        .and_then(|policy| policy.managed_activation.as_ref())
        .ok_or_else(|| anyhow::anyhow!("node has no managed external-content activation policy"))
}

fn external_content_policy(
    state: &AppState,
) -> Result<&ryeos_app::node_config::sections::external_content::ExternalContentImportPolicyRecord>
{
    state
        .node_config
        .external_content_import_policy
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("node has no external-content import policy"))
}

fn bounded_error(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    while output.len() > ERROR_LIMIT {
        output.pop();
    }
    let output = output.trim();
    if output.is_empty() {
        "managed external-content activation failed".to_owned()
    } else {
        output.to_owned()
    }
}

pub async fn recover_durable_activations(state: &AppState) -> Result<usize> {
    let jobs = state.state_store.with_state_db(|db| {
        db.list_active_sync_jobs_by_operation_type(MANAGED_ACTIVATION_OPERATION, 64)
    })?;
    let mut recovered = 0usize;
    for job in jobs {
        let operation = match ManagedActivationJobOperation::from_value(job.operation.clone()) {
            Ok(operation) => operation,
            Err(error) => {
                let detail = bounded_error(&format!(
                    "retained managed activation operation is invalid: {error:#}"
                ));
                state.state_store.with_state_db(|db| {
                    db.update_sync_job(
                        &job.job_id,
                        &ryeos_state::SyncJobUpdate {
                            state: ryeos_state::SyncJobState::Failed,
                            phase: "operation_invalid".to_owned(),
                            roots: None,
                            heads: None,
                            uploaded_hashes: job.uploaded_hashes.clone(),
                            fetched_hashes: job.fetched_hashes.clone(),
                            last_error: Some(detail),
                            result: job.result.clone(),
                        },
                    )
                })?;
                tracing::error!(job_id = %job.job_id, %error, "invalid managed activation operation terminalized");
                cleanup_retained_terminal_staging(state, &job.job_id, &job.operation).await;
                continue;
            }
        };
        let activation = match ryeos_app::managed_external_content::resolve_activation(
            state,
            &operation.activation_ref,
        ) {
            Ok(activation) => activation,
            Err(error) => {
                let detail = bounded_error(&format!(
                    "retained managed activation can no longer resolve its exact signed program: {error:#}"
                ));
                state.state_store.with_state_db(|db| {
                    db.update_sync_job(
                        &job.job_id,
                        &ryeos_state::SyncJobUpdate {
                            state: ryeos_state::SyncJobState::Failed,
                            phase: "program_unavailable".to_owned(),
                            roots: None,
                            heads: None,
                            uploaded_hashes: job.uploaded_hashes.clone(),
                            fetched_hashes: job.fetched_hashes.clone(),
                            last_error: Some(detail),
                            result: job.result.clone(),
                        },
                    )
                })?;
                cleanup_retained_terminal_staging(state, &job.job_id, &job.operation).await;
                continue;
            }
        };
        match execute_operation(Arc::new(state.clone()), activation, operation, false).await {
            Ok(_) => recovered += 1,
            Err(error) => {
                tracing::warn!(job_id = %job.job_id, %error, "managed activation recovery attempt did not complete")
            }
        }
    }
    Ok(recovered)
}

async fn cleanup_retained_terminal_staging(state: &AppState, job_id: &str, operation: &Value) {
    let operation_digest = match ryeos_state::objects::canonical_value_digest(operation) {
        Ok(digest) if activation_job_id(&digest) == job_id => digest,
        Ok(_) => {
            tracing::warn!(%job_id, "terminal managed activation job identity does not authorize staging cleanup");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, %job_id, "terminal managed activation operation cannot authorize staging cleanup");
            return;
        }
    };
    let state = state.clone();
    let directories = match tokio::task::spawn_blocking(move || {
        open_activation_directories(&state, &operation_digest)
    })
    .await
    {
        Ok(Ok(directories)) => directories,
        Ok(Err(error)) => {
            tracing::warn!(%error, %job_id, "terminal managed activation staging could not be opened for cleanup");
            return;
        }
        Err(error) => {
            tracing::warn!(%error, %job_id, "terminal managed activation staging cleanup task panicked");
            return;
        }
    };
    cleanup_terminal_staging(directories, job_id).await;
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/activate",
    endpoint: "external-content.activate",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/activate"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::managed_external_content::{
        ManagedActivationComponentMember, ManagedActivationMember, ManagedComponentStorage,
        ResolvedManagedActivationComponent,
    };

    fn test_policy()
    -> ryeos_app::node_config::sections::external_content::ManagedExternalContentActivationPolicy
    {
        ryeos_app::node_config::sections::external_content::ManagedExternalContentActivationPolicy {
            allow_online: false,
            allowed_https_hosts: vec!["releases.example.test".to_owned()],
            max_archives: 1,
            max_compressed_bytes: 4096,
            max_expanded_bytes: 4096,
            max_members: 8,
            max_member_bytes: 1024,
            max_concurrent_activations: 1,
            cache_budget_bytes: 8192,
            store_budget_bytes: 8192,
            minimum_free_bytes: 1,
            max_attempts: 2,
        }
    }

    fn test_activation(
        source: ManagedActivationSource,
    ) -> ResolvedManagedExternalContentActivation {
        ResolvedManagedExternalContentActivation {
            activation_ref: "config:fixture/activation".to_owned(),
            activation_program_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            document: ryeos_app::managed_external_content::ManagedExternalContentActivation {
                schema: ryeos_app::managed_external_content::MANAGED_ACTIVATION_SCHEMA.to_owned(),
                consumer_ref: "worker:fixture/hosted".to_owned(),
                sources: vec![source],
                components: vec![
                    ryeos_app::managed_external_content::ManagedActivationComponent {
                        id: "runtime".to_owned(),
                        storage: ManagedComponentStorage::LargeContent,
                        members: vec![ManagedActivationComponentMember {
                            source: "package".to_owned(),
                            member: "bin/runtime".to_owned(),
                            target: None,
                        }],
                    },
                ],
            },
            components: vec![ResolvedManagedActivationComponent {
                recipe: ryeos_app::managed_external_content::ManagedActivationComponent {
                    id: "runtime".to_owned(),
                    storage: ManagedComponentStorage::LargeContent,
                    members: vec![ManagedActivationComponentMember {
                        source: "package".to_owned(),
                        member: "bin/runtime".to_owned(),
                        target: None,
                    }],
                },
                expected_manifest_hash: "d".repeat(64),
                expected_manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                    .to_owned(),
                declaration_kind: ryeos_engine::external_content::ExternalContentKind::File,
            }],
        }
    }

    #[test]
    fn durable_job_identity_includes_invocation_authority() {
        let program_digest = "b".repeat(64);
        let consumer_ref = "worker:fixture/hosted".to_owned();
        let publisher_fingerprint = "c".repeat(64);
        let activation_id =
            ryeos_state::objects::ExternalContentActivationReceipt::derive_activation_id(
                &program_digest,
                &consumer_ref,
                &publisher_fingerprint,
            )
            .unwrap();
        let mut operation = ManagedActivationJobOperation {
            operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
            schema: "ryeos.external_content_activation_operation.v2".to_owned(),
            activation_ref: "config:fixture/activation".to_owned(),
            activation_program_digest: program_digest,
            activation_id: activation_id.clone(),
            consumer_ref,
            publisher_fingerprint,
            operator_fingerprint: "d".repeat(64),
            operator_authority_digest: "4".repeat(64),
            policy_digest: "e".repeat(64),
            acquisition_mode: AcquisitionMode::Online,
        };
        let first_digest =
            ryeos_state::objects::canonical_value_digest(&operation.to_value().unwrap()).unwrap();
        let first = activation_job_id(&first_digest);
        operation.policy_digest = "f".repeat(64);
        let later_digest =
            ryeos_state::objects::canonical_value_digest(&operation.to_value().unwrap()).unwrap();
        let later = activation_job_id(&later_digest);

        assert_ne!(first, later);
        assert!(first.len() <= 128);
        assert!(first.ends_with(&first_digest));
    }

    #[test]
    fn selected_archive_members_are_bounded_and_staged_by_consumer_id() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let bytes = b"runtime".to_vec();
        let digest = lillux::sha256_hex(&bytes);
        let archive_path = root.path().join("fixture.tar.gz");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, bytes.as_slice()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: "a".repeat(64),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: digest.clone(),
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let activation = test_activation(source.clone());
        let policy = test_policy();
        let archive = std::fs::File::open(&archive_path).unwrap();
        extract_selected_members(archive, &source, &activation, &staging, &policy).unwrap();
        let mut staged = staging
            .open_regular(OsStr::new("runtime"), false)
            .unwrap()
            .unwrap();
        verify_open_file(&mut staged, 64, &digest, "fixture").unwrap();
    }

    #[test]
    fn selected_archive_members_build_a_descriptor_rooted_tree() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let bytes = b"runtime".to_vec();
        let digest = lillux::sha256_hex(&bytes);
        let archive_file = root.path().join("tree.tar.gz");
        {
            let file = std::fs::File::create(&archive_file).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, bytes.as_slice()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/tree.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: "a".repeat(64),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: digest.clone(),
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let mut activation = test_activation(source.clone());
        activation.document.components[0].members[0].target = Some("tools/runtime".to_owned());
        activation.components[0].recipe = activation.document.components[0].clone();
        activation.components[0].declaration_kind =
            ryeos_engine::external_content::ExternalContentKind::Tree;
        let policy = test_policy();
        extract_selected_members(
            std::fs::File::open(&archive_file).unwrap(),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .unwrap();
        let component = staging
            .open_child_directory(OsStr::new("runtime"))
            .unwrap()
            .unwrap();
        let tools = component
            .open_child_directory(OsStr::new("tools"))
            .unwrap()
            .unwrap();
        let mut staged = tools
            .open_regular(OsStr::new("runtime"), false)
            .unwrap()
            .unwrap();
        verify_open_file(&mut staged, 64, &digest, "tree fixture").unwrap();
    }

    #[test]
    fn selected_archive_refuses_links_and_executable_mode_drift() {
        let root = tempfile::tempdir().unwrap();
        let staging = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let bytes = b"runtime".to_vec();
        let digest = lillux::sha256_hex(&bytes);
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: "a".repeat(64),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: digest,
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let activation = test_activation(source.clone());
        let policy = test_policy();

        let link_archive = root.path().join("link.tar.gz");
        {
            let file = std::fs::File::create(&link_archive).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("../outside").unwrap();
            header.set_size(0);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append(&header, std::io::empty()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let error = extract_selected_members(
            std::fs::File::open(&link_archive).unwrap(),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("managed activation must reject archive links");
        assert!(error.to_string().contains("link or special entry"));

        let mode_archive = root.path().join("mode.tar.gz");
        {
            let file = std::fs::File::create(&mode_archive).unwrap();
            let gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tar = tar::Builder::new(gzip);
            let mut header = tar::Header::new_gnu();
            header.set_path("bin/runtime").unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append(&header, bytes.as_slice()).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let error = extract_selected_members(
            std::fs::File::open(&mode_archive).unwrap(),
            &source,
            &activation,
            &staging,
            &policy,
        )
        .expect_err("managed activation must reject selected-member mode drift");
        assert!(error.to_string().contains("executable mode changed"));
    }

    #[test]
    fn invalid_cached_archive_is_removed_and_offline_activation_refuses() {
        let root = tempfile::tempdir().unwrap();
        let cache = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let expected = b"expected archive";
        let digest = lillux::sha256_hex(expected);
        std::fs::write(root.path().join(&digest), b"corrupt archive").unwrap();
        let source = ManagedActivationSource {
            id: "package".to_owned(),
            url: "https://releases.example.test/fixture.tar.gz".to_owned(),
            archive_format: ryeos_app::managed_external_content::MANAGED_ACTIVATION_ARCHIVE_FORMAT
                .to_owned(),
            sha256: digest.clone(),
            maximum_compressed_bytes: 4096,
            maximum_expanded_bytes: 4096,
            members: vec![ManagedActivationMember {
                path: "bin/runtime".to_owned(),
                disposition: ManagedMemberDisposition::Import,
                sha256: "a".repeat(64),
                maximum_bytes: 64,
                executable: true,
            }],
        };
        let policy = test_policy();

        let error = obtain_archive(&cache, &source, AcquisitionMode::Offline, &policy)
            .expect_err("offline activation must refuse corrupt cache content");
        assert!(format!("{error:#}").contains("failed exact verification"));
        assert!(
            cache
                .open_regular(OsStr::new(&digest), false)
                .unwrap()
                .is_none(),
            "invalid cache coordinate must be reusable by a later online retry"
        );
    }
}
