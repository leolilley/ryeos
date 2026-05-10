use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::state_store::{
    StateStore, FinalizeThreadRecord, NewArtifactRecord, NewThreadRecord, ThreadArtifactRecord,
    ThreadDetail, ThreadEdgeRecord, ThreadResultRecord,
};
use crate::kind_profiles::KindProfileRegistry;
use crate::services::event_store::EventStoreService;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, EngineContext, ExecutionArtifact, ExecutionCompletion, ExecutionHints,
    FinalCost, LaunchMode, PlanContext, Principal, ProjectContext, ResolvedItem,
    ThreadTerminalStatus, TrustClass,
};
use ryeos_engine::engine::Engine;

pub struct ThreadLifecycleService {
    state_store: Arc<StateStore>,
    kind_profiles: Arc<KindProfileRegistry>,
    _events: Arc<EventStoreService>,
    current_site_id: String,
    scheduler_db: std::sync::RwLock<Option<Arc<crate::scheduler::db::SchedulerDb>>>,
    system_space_dir: std::sync::RwLock<Option<std::path::PathBuf>>,
}

impl std::fmt::Debug for ThreadLifecycleService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadLifecycleService")
            .field("current_site_id", &self.current_site_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteResponseResult {
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub artifacts: Vec<ThreadArtifactRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadCreateParams {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    #[serde(default)]
    pub upstream_thread_id: Option<String>,
    #[serde(default)]
    pub requested_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadMarkRunningParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadAttachProcessParams {
    pub thread_id: String,
    pub pid: i64,
    pub pgid: i64,
    #[serde(default)]
    pub metadata: Option<Value>,
    /// Spawn-time metadata persisted alongside pid/pgid (cancellation
    /// policy, checkpoint dir, etc.). Defaults to empty so wire
    /// callers (UDS) that don't set it use the daemon defaults.
    #[serde(default)]
    pub launch_metadata: crate::launch_metadata::RuntimeLaunchMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGetParams {
    pub thread_id: String,
}

#[derive(Debug, Serialize)]
pub struct ThreadChainResult {
    pub threads: Vec<ThreadDetail>,
    pub edges: Vec<ThreadEdgeRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPublishParams {
    pub thread_id: String,
    pub artifact_type: String,
    pub uri: String,
    #[serde(default)]
    pub content_hash: Option<String>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadFinalizeParams {
    pub thread_id: String,
    pub status: String,
    #[serde(default)]
    pub outcome_code: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default)]
    pub artifacts: Vec<ExecutionArtifact>,
    #[serde(default)]
    pub final_cost: Option<FinalCost>,
    #[serde(default)]
    pub summary_json: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadContinuationParams {
    pub thread_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ThreadContinuationResult {
    pub source_thread_id: String,
    pub successor_thread_id: String,
    pub chain_root_id: String,
    pub successor: ThreadDetail,
}

#[derive(Debug, Clone)]
pub struct ResolvedExecutionRequest {
    pub kind: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub target_site_id: Option<String>,
    pub requested_by: Option<String>,
    pub parameters: Value,
    /// The engine's resolved item — carried through for verify/build_plan/execute.
    pub resolved_item: ResolvedItem,
    /// The PlanContext used during resolution — reused for verify/build_plan.
    pub plan_context: PlanContext,
}

impl ThreadLifecycleService {
    pub fn new(
        state_store: Arc<StateStore>,
        kind_profiles: Arc<KindProfileRegistry>,
        _events: Arc<EventStoreService>,
    ) -> anyhow::Result<Self> {
        let hostname = env::var("HOSTNAME")
            .map_err(|_| anyhow::anyhow!(
                "required environment variable HOSTNAME is not set. \
                 Set it to this node's identity (e.g. hostname or unique site ID). \
                 This is used to construct the site_id for thread isolation."
            ))?;
        if hostname.trim().is_empty() {
            anyhow::bail!(
                "HOSTNAME environment variable is set but empty. \
                 Set it to a non-empty value identifying this node."
            );
        }

        Ok(Self {
            state_store,
            kind_profiles,
            _events,
            current_site_id: format!("site:{hostname}"),
            scheduler_db: std::sync::RwLock::new(None),
            system_space_dir: std::sync::RwLock::new(None),
        })
    }

    /// Wire the scheduler DB for thread completion tracking.
    /// Called once after construction, once the scheduler DB is available.
    pub fn set_scheduler_db(&self, db: Arc<crate::scheduler::db::SchedulerDb>, system_space_dir: std::path::PathBuf) {
        *self.scheduler_db.write().unwrap() = Some(db);
        *self.system_space_dir.write().unwrap() = Some(system_space_dir);
    }

    pub fn kind_profiles(&self) -> &KindProfileRegistry {
        &self.kind_profiles
    }

    pub fn site_id(&self) -> &str {
        &self.current_site_id
    }

    #[tracing::instrument(
        level = "debug",
        name = "thread:create_root",
        skip(self, request),
        fields(
            kind = %request.kind,
            launch_mode = %request.launch_mode,
            item_ref = %request.item_ref,
        )
    )]
    pub fn create_root_thread(&self, request: &ResolvedExecutionRequest) -> Result<ThreadDetail> {
        self.create_root_thread_with_id(&new_thread_id(), request)
    }

    pub fn create_root_thread_with_id(
        &self,
        thread_id: &str,
        request: &ResolvedExecutionRequest,
    ) -> Result<ThreadDetail> {
        validate_kind(&request.kind, self.kind_profiles())?;
        validate_thread_id_format(thread_id)?;
        let thread_record = NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: thread_id.to_string(),
            kind: request.kind.clone(),
            item_ref: request.item_ref.clone(),
            executor_ref: request.executor_ref.clone(),
            launch_mode: request.launch_mode.clone(),
            current_site_id: request.current_site_id.clone(),
            origin_site_id: request.origin_site_id.clone(),
            upstream_thread_id: None,
            requested_by: request.requested_by.clone(),
        };

        let _persisted = self.state_store.create_thread(&thread_record)?;

        self.get_thread(thread_id)?
            .ok_or_else(|| anyhow!("created thread missing from database: {thread_id}"))
    }

    #[tracing::instrument(
        level = "debug",
        name = "thread:create",
        skip(self, params),
        fields(
            thread_id = %params.thread_id,
            chain_root_id = %params.chain_root_id,
            kind = %params.kind,
            launch_mode = %params.launch_mode,
        )
    )]
    pub fn create_thread(&self, params: &ThreadCreateParams) -> Result<ThreadDetail> {
        validate_kind(&params.kind, self.kind_profiles())?;
        validate_launch_mode(&params.launch_mode)?;

        let thread_record = NewThreadRecord {
            thread_id: params.thread_id.clone(),
            chain_root_id: params.chain_root_id.clone(),
            kind: params.kind.clone(),
            item_ref: params.item_ref.clone(),
            executor_ref: params.executor_ref.clone(),
            launch_mode: params.launch_mode.clone(),
            current_site_id: params.current_site_id.clone(),
            origin_site_id: params.origin_site_id.clone(),
            upstream_thread_id: params.upstream_thread_id.clone(),
            requested_by: params.requested_by.clone(),
        };

        let _persisted = self.state_store.create_thread(&thread_record)?;

        self.get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("created thread missing from database: {}", params.thread_id))
    }

    #[tracing::instrument(
        level = "debug",
        name = "thread:mark_running",
        skip_all,
        fields(thread_id = %thread_id)
    )]
    pub fn mark_running(&self, thread_id: &str) -> Result<ThreadDetail> {
        let _persisted = self.state_store.mark_thread_running(thread_id, None)?;
        self.get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found after mark_running: {thread_id}"))
    }

    #[tracing::instrument(
        level = "debug",
        name = "thread:attach_process",
        skip(self, params),
        fields(
            thread_id = %params.thread_id,
            pid = params.pid,
            pgid = params.pgid,
        )
    )]
    pub fn attach_process(&self, params: &ThreadAttachProcessParams) -> Result<ThreadDetail> {
        self.state_store.attach_thread_process(
            &params.thread_id,
            params.pid,
            params.pgid,
            &params.launch_metadata,
        )?;
        self.get_thread(&params.thread_id)?.ok_or_else(|| {
            anyhow!(
                "thread not found after attach_process: {}",
                params.thread_id
            )
        })
    }

    #[tracing::instrument(
        level = "debug",
        name = "thread:finalize_from_completion",
        skip_all,
        fields(thread_id = %thread_id)
    )]
    pub fn finalize_from_completion(
        &self,
        thread_id: &str,
        completion: &ExecutionCompletion,
    ) -> Result<ThreadDetail> {
        let terminal_status = match completion.status {
            ThreadTerminalStatus::Completed => "completed",
            ThreadTerminalStatus::Failed => "failed",
            ThreadTerminalStatus::Cancelled => "cancelled",
            ThreadTerminalStatus::Continued => "continued",
            ThreadTerminalStatus::Killed => "killed",
        };
        let outcome_code = completion.outcome_code.clone().or_else(|| {
            Some(if terminal_status == "completed" {
                "success".to_string()
            } else {
                terminal_status.to_string()
            })
        });

        let _persisted = self.state_store.finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: terminal_status.to_string(),
                outcome_code,
                result_json: completion.result.clone(),
                error_json: completion.error.clone(),
                artifacts: completion
                    .artifacts
                    .iter()
                    .map(artifact_to_record)
                    .collect(),
                final_cost: completion.final_cost.clone(),
            },
        )?;

        let finalized = self
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {thread_id}"))?;

        // Handle continuation request from runtime
        if let Some(cr) = &completion.continuation_request {
            if terminal_status == "completed" {
                match self.request_continuation(&ThreadContinuationParams {
                    thread_id: thread_id.to_string(),
                    reason: Some(cr.reason.clone()),
                }) {
                    Ok(_continuation) => {
                        // Source thread is now "continued", return updated state
                        return self.get_thread(thread_id)?.ok_or_else(|| {
                            anyhow!("thread not found after continuation: {thread_id}")
                        });
                    }
                    Err(err) => {
                        tracing::warn!(
                            thread_id = %thread_id,
                            error = %err,
                            "continuation request failed"
                        );
                        // Fall through — thread stays in its finalized state
                    }
                }
            }
        }

        Ok(finalized)
    }

    #[tracing::instrument(
        level = "debug",
        name = "thread:finalize",
        skip(self, params),
        fields(
            thread_id = %params.thread_id,
            status = %params.status,
        )
    )]
    pub fn finalize_thread(&self, params: &ThreadFinalizeParams) -> Result<ThreadDetail> {
        let _persisted = self.state_store.finalize_thread(
            &params.thread_id,
            &FinalizeThreadRecord {
                status: normalize_terminal_status(&params.status)?.to_string(),
                outcome_code: params.outcome_code.clone(),
                result_json: params.result.clone(),
                error_json: params.error.clone(),
                artifacts: params.artifacts.iter().map(artifact_to_record).collect(),
                final_cost: params.final_cost.clone(),
            },
        )?;

        // Update scheduler fire record if this thread was scheduler-dispatched.
        if let Ok(guard) = self.scheduler_db.read() {
            if let Some(ref db) = *guard {
                if let Ok(Some(fire)) = db.find_fire_by_thread(&params.thread_id) {
                    let terminal_status = normalize_terminal_status(&params.status)?;
                    let fire_status = if terminal_status == "completed" {
                        "completed"
                    } else {
                        "failed"
                    };
                    let outcome_str = params.outcome_code.as_deref().unwrap_or(fire_status);
                    let now = lillux::time::timestamp_millis();

                    // Clone fields needed for JSONL before the move
                    let jsonl_fire_id = fire.fire_id.clone();
                    let jsonl_schedule_id = fire.schedule_id.clone();
                    let jsonl_scheduled_at = fire.scheduled_at;
                    let jsonl_fired_at = fire.fired_at;
                    let jsonl_signer_fp = fire.signer_fingerprint.clone();

                    let updated = crate::scheduler::types::FireRecord {
                        status: fire_status.to_string(),
                        outcome: Some(outcome_str.to_string()),
                        fired_at: Some(now),
                        ..fire
                    };
                    if let Err(e) = db.upsert_fire(&updated) {
                        tracing::warn!(
                            thread_id = %params.thread_id,
                            error = %e,
                            "scheduler: failed to update fire status on thread completion"
                        );
                    }

                    // Append completion to JSONL
                    if let Ok(dir_guard) = self.system_space_dir.read() {
                        if let Some(ref sys_dir) = *dir_guard {
                            let entry = serde_json::json!({
                                "entry_type": fire_status,
                                "fire_id": jsonl_fire_id,
                                "schedule_id": jsonl_schedule_id,
                                "scheduled_at": jsonl_scheduled_at,
                                "fired_at": jsonl_fired_at,
                                "thread_id": params.thread_id,
                                "completed_at": now,
                                "outcome": outcome_str,
                                "signer_fingerprint": jsonl_signer_fp,
                            });
                            let fires_path = sys_dir
                                .join(ryeos_engine::AI_DIR).join("state").join("schedules")
                                .join(&jsonl_schedule_id).join("fires.jsonl");
                            if let Err(e) = crate::scheduler::projection::append_jsonl_entry(&fires_path, &entry) {
                                tracing::warn!(
                                    thread_id = %params.thread_id,
                                    error = %e,
                                    "scheduler: failed to append completion to JSONL"
                                );
                            }
                        }
                    }
                }
            }
        }

        self.get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {}", params.thread_id))
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        self.state_store.get_thread(thread_id)
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        self.state_store.get_thread_result(thread_id)
    }

    pub fn list_thread_artifacts(&self, thread_id: &str) -> Result<Vec<ThreadArtifactRecord>> {
        self.state_store.list_thread_artifacts(thread_id)
    }

    pub fn build_execute_result(&self, thread_id: &str) -> Result<Option<ExecuteResponseResult>> {
        let result = self.state_store.get_thread_result(thread_id)?;
        let artifacts = self.state_store.list_thread_artifacts(thread_id)?;
        Ok(result.map(|result| ExecuteResponseResult {
            outcome_code: result.outcome_code,
            result: result.result,
            error: result.error,
            artifacts,
        }))
    }

    #[tracing::instrument(
        level = "debug",
        name = "thread:request_continuation",
        skip(self, params),
        fields(thread_id = %params.thread_id)
    )]
    pub fn request_continuation(
        &self,
        params: &ThreadContinuationParams,
    ) -> Result<ThreadContinuationResult> {
        let source = self
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {}", params.thread_id))?;

        if source.status != "running" && source.status != "completed" && source.status != "failed" {
            bail!(
                "cannot continue thread in status '{}' (must be running, completed, or failed)",
                source.status
            );
        }

        let profile = self.kind_profiles().get(&source.kind);
        if !profile.is_some_and(|p| p.supports_continuation) {
            bail!("continuation is not supported for kind '{}'", source.kind);
        }

        // Create successor thread in the same chain
        let successor_id = new_thread_id();
        let successor_record = NewThreadRecord {
            thread_id: successor_id.clone(),
            chain_root_id: source.chain_root_id.clone(),
            kind: source.kind.clone(),
            item_ref: source.item_ref.clone(),
            executor_ref: source.executor_ref.clone(),
            launch_mode: source.launch_mode.clone(),
            current_site_id: source.current_site_id.clone(),
            origin_site_id: source.origin_site_id.clone(),
            upstream_thread_id: Some(source.thread_id.clone()),
            requested_by: source.requested_by.clone(),
        };

        // Create successor, write continued edge, finalize source — all in one transaction
        let _persisted = self.state_store.create_continuation(
            &successor_record,
            &source.thread_id,
            &source.chain_root_id,
            params.reason.as_deref(),
        )?;

        let successor = self
            .get_thread(&successor_id)?
            .ok_or_else(|| anyhow!("successor thread missing after creation: {successor_id}"))?;

        Ok(ThreadContinuationResult {
            source_thread_id: source.thread_id,
            successor_thread_id: successor.thread_id.clone(),
            chain_root_id: source.chain_root_id,
            successor,
        })
    }

    #[tracing::instrument(
        level = "debug",
        name = "artifact:publish",
        skip(self, params),
        fields(
            thread_id = %params.thread_id,
            artifact_type = %params.artifact_type,
        )
    )]
    pub fn publish_artifact(&self, params: &ArtifactPublishParams) -> Result<ThreadArtifactRecord> {
        let (artifact, _persisted) = self.state_store.publish_artifact(
            &params.thread_id,
            &NewArtifactRecord {
                artifact_type: params.artifact_type.clone(),
                uri: params.uri.clone(),
                content_hash: params.content_hash.clone(),
                metadata: params.metadata.clone(),
            },
        )?;
        Ok(artifact)
    }

    pub fn list_threads(&self, limit: usize) -> Result<Value> {
        Ok(json!({
            "threads": self.state_store.list_threads(limit)?,
            "next_cursor": null,
        }))
    }

    pub fn list_children(&self, thread_id: &str) -> Result<Vec<ThreadDetail>> {
        self.state_store.list_thread_children(thread_id)
    }

    pub fn get_chain(&self, thread_id: &str) -> Result<Option<ThreadChainResult>> {
        let Some(thread) = self.get_thread(thread_id)? else {
            return Ok(None);
        };

        Ok(Some(ThreadChainResult {
            threads: self.state_store.list_chain_threads(&thread.chain_root_id)?,
            edges: self.state_store.list_chain_edges(&thread.chain_root_id)?,
        }))
    }
}

fn normalize_terminal_status(status: &str) -> Result<&str> {
    match status {
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued" => Ok(status),
        other => bail!("invalid terminal status: {other}"),
    }
}

fn validate_kind(kind: &str, profiles: &KindProfileRegistry) -> Result<()> {
    if profiles.is_valid(kind) {
        Ok(())
    } else {
        bail!("invalid thread kind: {kind}")
    }
}

fn validate_launch_mode(launch_mode: &str) -> Result<()> {
    match launch_mode {
        "inline" | "detached" => Ok(()),
        other => bail!("invalid launch mode: {other}"),
    }
}

fn artifact_to_record(artifact: &ExecutionArtifact) -> NewArtifactRecord {
    NewArtifactRecord {
        artifact_type: artifact.artifact_type.clone(),
        uri: artifact.uri.clone(),
        content_hash: artifact.content_hash.clone(),
        metadata: artifact.metadata.clone(),
    }
}

fn validate_thread_id_format(id: &str) -> Result<()> {
    if !id.starts_with("T-") {
        bail!("thread_id must start with `T-`: got `{id}`");
    }
    let suffix = &id[2..];
    let segments: Vec<&str> = suffix.split('-').collect();
    if segments.len() != 5 {
        bail!(
            "thread_id suffix must have 5 dash-separated hex groups: got `{suffix}`"
        );
    }
    let expected_lengths: &[usize] = &[8, 4, 4, 4, 12];
    for (seg, &expected) in segments.iter().zip(expected_lengths.iter()) {
        if seg.len() != expected || !seg.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(
                "thread_id suffix hex groups must have lengths {expected_lengths:?}: got `{suffix}`"
            );
        }
    }
    Ok(())
}

/// Mints a fresh `T-{uuid}` thread id. 16 random bytes from `OsRng`.
/// Collision probability is operationally negligible; `state_store`
/// has `thread_id TEXT PRIMARY KEY` so an unlikely duplicate fails loudly.
pub fn new_thread_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!(
        "T-{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

/// Resolve a canonical item ref through the engine.
pub struct ResolveRootExecutionParams<'a> {
    pub engine: &'a Engine,
    pub site_id: &'a str,
    pub project_path: &'a Path,
    pub item_ref: &'a str,
    pub launch_mode: &'a str,
    pub parameters: Value,
    pub requested_by: Option<String>,
    pub caller_scopes: Vec<String>,
    pub validate_only: bool,
}

pub fn resolve_root_execution(params: ResolveRootExecutionParams<'_>) -> Result<ResolvedExecutionRequest> {
    let ResolveRootExecutionParams {
        engine,
        site_id,
        project_path,
        item_ref,
        launch_mode,
        parameters,
        requested_by,
        caller_scopes,
        validate_only,
    } = params;
    let project_path = project_path.to_path_buf();

    let canonical_ref =
        CanonicalRef::parse(item_ref).map_err(|e| anyhow!("invalid item ref: {e}"))?;

    validate_launch_mode(launch_mode)?;

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: requested_by.clone().unwrap_or_else(|| "fp:local".into()),
            scopes: caller_scopes,
        }),
        project_context: ProjectContext::LocalPath { path: project_path },
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        execution_hints: ExecutionHints::default(),
        validate_only,
    };

    let resolved = engine
        .resolve(&plan_ctx, &canonical_ref)
        .map_err(|e| anyhow!("resolution failed: {e}"))?;

    let thread_kind = map_to_thread_kind(&resolved.kind);

    let executor_ref = resolved
        .metadata
        .executor_id
        .clone()
        .ok_or_else(|| {
            anyhow!(
                "item {} does not declare an executor_id",
                item_ref
            )
        })?;

    Ok(ResolvedExecutionRequest {
        kind: thread_kind,
        item_ref: item_ref.to_string(),
        executor_ref,
        launch_mode: launch_mode.to_string(),
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        target_site_id: None,
        requested_by,
        parameters,
        resolved_item: resolved,
        plan_context: plan_ctx,
    })
}

/// Result of dry-run validation (verify + trust + build_plan, no spawn).
pub struct ValidatedItem {
    pub trust_class: TrustClass,
    pub plan_id: String,
}

/// Run verify → trust → build_plan without spawning.
pub fn validate_item(
    engine: &Engine,
    resolved: &ResolvedExecutionRequest,
) -> Result<ValidatedItem> {
    let verified = engine
        .verify(&resolved.plan_context, resolved.resolved_item.clone())
        .map_err(|e| anyhow!("verification failed: {e}"))?;

    let plan = engine
        .build_plan(
            &resolved.plan_context,
            &verified,
            &resolved.parameters,
            &resolved.plan_context.execution_hints,
        )
        .map_err(|e| anyhow!("plan build failed: {e}"))?;

    Ok(ValidatedItem {
        trust_class: verified.trust_class,
        plan_id: plan.plan_id,
    })
}

/// Result of spawning the engine pipeline.
pub struct SpawnedItem {
    pub pid: u32,
    pub pgid: i64,
    /// Spawn-time metadata derived from the engine `SubprocessSpec`
    /// (e.g. `native_async` cancellation policy). Persisted alongside
    /// pid/pgid so the daemon shutdown / cancel paths can route
    /// termination without re-loading the spec.
    pub launch_metadata: crate::launch_metadata::RuntimeLaunchMetadata,
    spawned: ryeos_engine::dispatch::SpawnedExecution,
}

impl SpawnedItem {
    /// Block until subprocess completes.
    pub fn wait(self) -> ExecutionCompletion {
        self.spawned.wait()
    }
}

/// Run the engine pipeline: verify → build_plan → spawn.
/// Returns a handle with pid/pgid that the daemon can persist before calling wait().
///
/// If `thread_state_dir` is supplied AND the resolved spec declares
/// `native_resume`, the daemon-side checkpoint directory
/// (`<thread_state_dir>/checkpoints/`) is created and injected as
/// `RYEOS_CHECKPOINT_DIR` into the subprocess env. The path is also
/// captured in `SpawnedItem.launch_metadata.checkpoint_dir` so the
/// daemon can persist it for the resume path. When `is_resume = true`,
/// `RYEOS_RESUME=1` is also injected so replay-aware tools can branch
/// on cold-start vs. resume.
pub struct SpawnItemParams<'a> {
    pub engine: &'a Engine,
    pub resolved: &'a ResolvedExecutionRequest,
    pub thread_id: &'a str,
    pub chain_root_id: &'a str,
    pub vault_bindings: std::collections::HashMap<String, String>,
    pub daemon_callback_env: std::collections::HashMap<String, String>,
    pub thread_state_dir: Option<&'a std::path::Path>,
    pub is_resume: bool,
    pub original_snapshot_hash: Option<&'a str>,
}

#[tracing::instrument(
    name = "thread:spawn",
    skip(params),
    fields(
        thread_id = %params.thread_id,
        chain_root_id = %params.chain_root_id,
        item_ref = %params.resolved.item_ref,
        is_resume = params.is_resume,
        snapshot_pinned = params.original_snapshot_hash.is_some(),
    )
)]
pub fn spawn_item(params: SpawnItemParams<'_>) -> Result<SpawnedItem> {
    let SpawnItemParams {
        engine,
        resolved,
        thread_id,
        chain_root_id,
        vault_bindings,
        daemon_callback_env,
        thread_state_dir,
        is_resume,
        original_snapshot_hash,
    } = params;
    // vault_bindings: user-provided secret/capability env vars.
    // daemon_callback_env: daemon infrastructure env (socket path, callback
    // token, thread id, project path). Sourced from AppState by the caller
    // (runner.rs), NOT from the daemon's own process env.
    let verified = engine
        .verify(&resolved.plan_context, resolved.resolved_item.clone())
        .map_err(|e| anyhow!("verification failed: {e}"))?;

    let mut plan = engine
        .build_plan(
            &resolved.plan_context,
            &verified,
            &resolved.parameters,
            &resolved.plan_context.execution_hints,
        )
        .map_err(|e| anyhow!("plan build failed: {e}"))?;

    // Inject the daemon's subprocess env contract into every subprocess
    // node: allowlisted parent env (PATH/HOME/...) + daemon-resolved
    // roots (USER_SPACE/RYEOS_SYSTEM_SPACE_DIR) + declared secrets, then
    // layer the daemon callback infra (socket path, callback token,
    // thread id, project path) on top. Mirrors `execution::launch::
    // spawn_runtime`'s composition so engine-dispatched `bin:` items
    // (e.g. `bin:ryos-core-tools`) see the same env contract as runtime-
    // binary spawns. Without this, `lillux::run`'s post-Part-B
    // `env_clear()` would leave the subprocess with only a handful of
    // RYEOSD_* vars and `ryeos_engine::roots::user_root()` would fail
    // to resolve in the child.
    let secret_map: std::collections::BTreeMap<String, String> = vault_bindings
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let base_env = crate::process::build_spawn_env(&secret_map)
        .map_err(|e| anyhow!("build subprocess env contract: {e}"))?;
    for node in &mut plan.nodes {
        if let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = node {
            // Seed allowlisted parent env + daemon-resolved roots +
            // declared secrets. `insert` overwrites parent-snapshotted
            // values with daemon-resolved values; plan-builder-set
            // RYE_* vars (RYEOS_ITEM_KIND, RYEOS_SITE_ID, ...) are
            // preserved because they don't collide with the allowlist.
            for (k, v) in &base_env {
                spec.env.insert(k.clone(), v.clone());
            }
            // Layer daemon callback env last — daemon-controlled infra
            // must always win over anything else.
            spec.env.extend(daemon_callback_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
    }

    // Allocate the per-thread checkpoint directory and inject
    // RYEOS_CHECKPOINT_DIR / RYEOS_RESUME into the subprocess env when the
    // spec declares `native_resume`. This is intentionally done at
    // spawn time rather than in the engine handler because the path
    // depends on the daemon-owned `<thread_state_dir>`. See
    // `RuntimeLaunchMetadata::checkpoint_dir`.
    let mut allocated_checkpoint_dir: Option<std::path::PathBuf> = None;
    if let Some(ts_dir) = thread_state_dir {
        for node in &mut plan.nodes {
            if let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = node
            {
                if spec.execution.native_resume.is_some() {
                    let ckpt = ts_dir.join("checkpoints");
                    std::fs::create_dir_all(&ckpt).map_err(|e| {
                        anyhow!(
                            "failed to create checkpoint dir {}: {e}",
                            ckpt.display()
                        )
                    })?;
                    spec.env.insert(
                        "RYEOS_CHECKPOINT_DIR".to_string(),
                        ckpt.display().to_string(),
                    );
                    if is_resume {
                        spec.env.insert("RYEOS_RESUME".to_string(), "1".to_string());
                    }
                    allocated_checkpoint_dir = Some(ckpt);
                    break; // first DispatchSubprocess wins, mirrors FirstWins
                }
            }
        }
    }

    let engine_ctx = EngineContext {
        thread_id: thread_id.to_string(),
        chain_root_id: chain_root_id.to_string(),
        current_site_id: resolved.current_site_id.clone(),
        origin_site_id: resolved.origin_site_id.clone(),
        upstream_site_id: None,
        upstream_thread_id: None,
        continuation_from_id: None,
        requested_by: resolved.plan_context.requested_by.clone(),
        project_context: resolved.plan_context.project_context.clone(),
        launch_mode: if resolved.launch_mode == "detached" {
            LaunchMode::Detached
        } else {
            LaunchMode::Inline
        },
    };

    // Derive spawn-time launch metadata from the first DispatchSubprocess
    // node before handing the plan off to the engine. The engine remains
    // canonical for engine-known data (in `SubprocessSpec`); this snapshots
    // the daemon-relevant slice so shutdown/cancel can route without
    // re-loading the spec.
    let mut launch_metadata = plan
        .nodes
        .iter()
        .find_map(|n| match n {
            ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } => Some(
                crate::launch_metadata::RuntimeLaunchMetadata::from_spec(spec),
            ),
            _ => None,
        })
        .unwrap_or_default();
    if let Some(ckpt) = allocated_checkpoint_dir {
        launch_metadata = launch_metadata.with_checkpoint_dir(ckpt);
    }
    // Capture resume context iff this thread declared native_resume.
    // `reconcile.rs` reads it on daemon restart to re-spawn the thread
    // under the same `thread_id` with `RYEOS_RESUME=1`.
    //
    // **Pinned-snapshot policy:** copy the full original
    // `PlanContext.project_context` AND the runner-allocated
    // `original_snapshot_hash` (if any). On resume, the reconciler
    // prefers a `ProjectContext::SnapshotHash { hash }` form when
    // `original_snapshot_hash` is `Some`, so resume runs against the
    // exact project version captured at spawn time, not the current
    // working-dir head. See `docs/future/RESUME-ADVANCED-PATH.md`.
    if launch_metadata.native_resume.is_some() {
        launch_metadata = launch_metadata.with_resume_context(
            crate::launch_metadata::ResumeContext {
                kind: resolved.kind.clone(),
                item_ref: resolved.item_ref.clone(),
                launch_mode: resolved.launch_mode.clone(),
                parameters: resolved.parameters.clone(),
                project_context: resolved.plan_context.project_context.clone(),
                original_snapshot_hash: original_snapshot_hash.map(str::to_string),
                current_site_id: resolved.plan_context.current_site_id.clone(),
                origin_site_id: resolved.plan_context.origin_site_id.clone(),
                requested_by: resolved.plan_context.requested_by.clone(),
                execution_hints: resolved.plan_context.execution_hints.clone(),
                // V5.5 P2: subprocess terminator has no permissions
                // composition step, so resumed callbacks inherit the
                // same deny-all posture the original spawn had. Native
                // runtime spawns that DO have a permissions model go
                // through `launch::build_and_launch`, not `spawn_item`.
                effective_caps: Vec::new(),
            },
        );
    }
    let spawned = engine
        .spawn_plan(&engine_ctx, &plan)
        .map_err(|e| anyhow!("spawn failed: {e}"))?;

    Ok(SpawnedItem {
        pid: spawned.pid,
        pgid: spawned.pgid,
        launch_metadata,
        spawned,
    })
}

/// Map a canonical item kind to the daemon's thread kind for profiling.
/// Convention: thread kind = "{item_kind}_run" unless the kind profile
/// registry has a more specific mapping.
fn map_to_thread_kind(canonical_kind: &str) -> String {
    format!("{canonical_kind}_run")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_thread_id_accepts_valid_format() {
        assert!(validate_thread_id_format("T-01234567-abcd-ef01-2345-6789abcdef01").is_ok());
        let id = new_thread_id();
        assert!(validate_thread_id_format(&id).is_ok());
    }

    #[test]
    fn validate_thread_id_rejects_missing_prefix() {
        let err = validate_thread_id_format("foo-123").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("must start with `T-`"), "got: {msg}");
    }

    #[test]
    fn validate_thread_id_rejects_non_uuid_suffix() {
        let err = validate_thread_id_format("T-not-a-uuid").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("hex groups"), "got: {msg}");
    }

    #[test]
    fn validate_thread_id_rejects_wrong_segment_lengths() {
        let err = validate_thread_id_format("T-01234567-ab-cdef-0123-456789abcdef01").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("hex groups"), "got: {msg}");
    }

    #[test]
    fn validate_thread_id_rejects_non_hex_chars() {
        let err = validate_thread_id_format("T-ghijklmn-abcd-ef01-2345-6789abcdef01").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("hex groups"), "got: {msg}");
    }
}
