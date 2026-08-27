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
    let operation = ManagedActivationJobOperation::new(&activation, operator, policy, req.mode)?;
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
    let job_id = format!(
        "external-content-activation:{}:{}",
        operation.activation_id,
        match operation.acquisition_mode {
            AcquisitionMode::Online => "online",
            AcquisitionMode::Offline => "offline",
        }
    );
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
        response.idempotent = true;
        return Ok(response);
    }
    if matches!(
        existing.state,
        ryeos_state::SyncJobState::Failed | ryeos_state::SyncJobState::Cancelled
    ) {
        bail!(
            "managed activation job {} is terminal in state {}: {}",
            job_id,
            existing.state.as_str(),
            existing
                .last_error
                .unwrap_or_else(|| "no retained diagnostic".to_owned())
        );
    }
    operation.validate_current(&activation, managed_policy(&state)?)?;

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
            Err(error)
        }
    }
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
    operation.validate_current(activation, managed_policy(state)?)?;
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
        let member = activation.member(component)?;
        let response = ryeos_app::operator_external_content::import_managed_activation_component(
            state,
            &operation.operator_fingerprint,
            activation,
            component,
            member,
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
            max_depth: 1,
            max_entries: CACHE_ENTRY_LIMIT,
        })?;
    if !directories
        .staging
        .remove_empty_child_if_same(OsStr::new(&directories.job_name), &directories.job)?
    {
        bail!("managed activation staging remained non-empty after bounded cleanup");
    }
    Ok(())
}

fn obtain_archive(
    cache: &lillux::PinnedDirectory,
    source: &ManagedActivationSource,
    mode: AcquisitionMode,
    policy: &ryeos_app::node_config::sections::external_content::ManagedExternalContentActivationPolicy,
) -> Result<std::fs::File> {
    let name = OsStr::new(&source.sha256);
    if let Some(mut existing) = cache.open_regular(name, false)? {
        verify_open_file(
            &mut existing,
            source.maximum_compressed_bytes,
            &source.sha256,
            "cached managed activation archive",
        )?;
        cache.ensure_path_binding()?;
        return Ok(existing);
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
        .filter(|component| component.recipe.source == source.id)
        .map(|component| (component.recipe.member.as_str(), component))
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
        let component = imported.get(path).ok_or_else(|| {
            anyhow::anyhow!("imported managed activation member has no resolved consumer component")
        })?;
        let mode = if member.executable { 0o755 } else { 0o644 };
        let name = OsStr::new(&component.recipe.id);
        let created = staging.atomic_create_regular_from_reader(
            name,
            &mut entry,
            member.maximum_bytes,
            mode,
        )?;
        let mut file = match created {
            Some((file, _)) => file,
            None => staging
                .open_regular(name, false)?
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
                tracing::error!(job_id = %job.job_id, %error, "invalid managed activation operation retained for operator inspection");
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
        ManagedActivationMember, ManagedComponentStorage, ResolvedManagedActivationComponent,
    };

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
        let activation = ResolvedManagedExternalContentActivation {
            activation_ref: "config:fixture/activation".to_owned(),
            activation_program_digest: "b".repeat(64),
            publisher_fingerprint: "c".repeat(64),
            document: ryeos_app::managed_external_content::ManagedExternalContentActivation {
                schema: ryeos_app::managed_external_content::MANAGED_ACTIVATION_SCHEMA.to_owned(),
                consumer_ref: "worker:fixture/hosted".to_owned(),
                sources: vec![source.clone()],
                components: vec![
                    ryeos_app::managed_external_content::ManagedActivationComponent {
                        id: "runtime".to_owned(),
                        source: "package".to_owned(),
                        member: "bin/runtime".to_owned(),
                        storage: ManagedComponentStorage::LargeContent,
                    },
                ],
            },
            components: vec![ResolvedManagedActivationComponent {
                recipe: ryeos_app::managed_external_content::ManagedActivationComponent {
                    id: "runtime".to_owned(),
                    source: "package".to_owned(),
                    member: "bin/runtime".to_owned(),
                    storage: ManagedComponentStorage::LargeContent,
                },
                expected_manifest_hash: "d".repeat(64),
                expected_manifest_kind: ryeos_state::objects::EXTERNAL_LARGE_CONTENT_MANIFEST_KIND
                    .to_owned(),
                declaration_kind: ryeos_engine::external_content::ExternalContentKind::File,
            }],
        };
        let policy =
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
            };
        let archive = std::fs::File::open(&archive_path).unwrap();
        extract_selected_members(archive, &source, &activation, &staging, &policy).unwrap();
        let mut staged = staging
            .open_regular(OsStr::new("runtime"), false)
            .unwrap()
            .unwrap();
        verify_open_file(&mut staged, 64, &digest, "fixture").unwrap();
    }
}
