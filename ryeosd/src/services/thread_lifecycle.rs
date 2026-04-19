use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::{
    Database, FinalizeThreadRecord, NewArtifactRecord, NewThreadRecord, ThreadArtifactRecord,
    ThreadDetail, ThreadEdgeRecord, ThreadResultRecord,
};
use crate::kind_profiles::KindProfileRegistry;
use crate::services::event_store::EventStoreService;
use rye_engine::canonical_ref::CanonicalRef;
use rye_engine::contracts::{
    EffectivePrincipal, EngineContext, ExecutionArtifact, ExecutionCompletion, ExecutionHints,
    FinalCost, LaunchMode, PlanContext, Principal, ProjectContext, ResolvedItem,
    ThreadTerminalStatus, TrustClass,
};
use rye_engine::engine::Engine;

#[derive(Debug, Clone)]
pub struct ThreadLifecycleService {
    db: Arc<Database>,
    events: Arc<EventStoreService>,
    current_site_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteResponseResult {
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub artifacts: Vec<ThreadArtifactRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct ThreadMarkRunningParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadAttachProcessParams {
    pub thread_id: String,
    pub pid: i64,
    pub pgid: i64,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadGetParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadListParams {
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadChildrenParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadChainParams {
    pub thread_id: String,
}

#[derive(Debug, Serialize)]
pub struct ThreadChainResult {
    pub threads: Vec<ThreadDetail>,
    pub edges: Vec<ThreadEdgeRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

fn default_list_limit() -> usize {
    20
}

impl ThreadLifecycleService {
    pub fn new(db: Arc<Database>, events: Arc<EventStoreService>) -> Self {
        let hostname = env::var("HOSTNAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());

        Self {
            db,
            events,
            current_site_id: format!("site:{hostname}"),
        }
    }

    pub fn kind_profiles(&self) -> &KindProfileRegistry {
        self.db.kind_profiles()
    }

    pub fn site_id(&self) -> &str {
        &self.current_site_id
    }

    pub fn create_root_thread(&self, request: &ResolvedExecutionRequest) -> Result<ThreadDetail> {
        validate_kind(&request.kind, self.kind_profiles())?;
        let thread_id = new_thread_id();
        let persisted = self.db.create_thread(
            &NewThreadRecord {
                thread_id: thread_id.clone(),
                chain_root_id: thread_id.clone(),
                kind: request.kind.clone(),
                status: "created".to_string(),
                item_ref: request.item_ref.clone(),
                executor_ref: request.executor_ref.clone(),
                launch_mode: request.launch_mode.clone(),
                current_site_id: request.current_site_id.clone(),
                origin_site_id: request.origin_site_id.clone(),
                upstream_thread_id: None,
                requested_by: request.requested_by.clone(),
                summary_json: None,
            },
            None,
        )?;
        self.events.publish_persisted_batch(&persisted);

        self.get_thread(&thread_id)?
            .ok_or_else(|| anyhow!("created thread missing from database: {thread_id}"))
    }

    pub fn create_thread(&self, params: &ThreadCreateParams) -> Result<ThreadDetail> {
        validate_kind(&params.kind, self.kind_profiles())?;
        validate_launch_mode(&params.launch_mode)?;

        let persisted = self.db.create_thread(
            &NewThreadRecord {
                thread_id: params.thread_id.clone(),
                chain_root_id: params.chain_root_id.clone(),
                kind: params.kind.clone(),
                status: "created".to_string(),
                item_ref: params.item_ref.clone(),
                executor_ref: params.executor_ref.clone(),
                launch_mode: params.launch_mode.clone(),
                current_site_id: params.current_site_id.clone(),
                origin_site_id: params.origin_site_id.clone(),
                upstream_thread_id: params.upstream_thread_id.clone(),
                requested_by: params.requested_by.clone(),
                summary_json: None,
            },
            None,
        )?;
        self.events.publish_persisted_batch(&persisted);

        self.get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("created thread missing from database: {}", params.thread_id))
    }

    pub fn mark_running(&self, thread_id: &str) -> Result<ThreadDetail> {
        let persisted = self.db.mark_thread_running(thread_id)?;
        self.events.publish_persisted_batch(&persisted);
        self.get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found after mark_running: {thread_id}"))
    }

    pub fn attach_process(&self, params: &ThreadAttachProcessParams) -> Result<ThreadDetail> {
        self.db.attach_thread_process(
            &params.thread_id,
            params.pid,
            params.pgid,
            params.metadata.as_ref(),
        )?;
        self.get_thread(&params.thread_id)?.ok_or_else(|| {
            anyhow!(
                "thread not found after attach_process: {}",
                params.thread_id
            )
        })
    }

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

        let persisted = self.db.finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: terminal_status.to_string(),
                outcome_code,
                result_json: completion.result.clone(),
                error_json: completion.error.clone(),
                metadata: completion.metadata.clone(),
                artifacts: completion
                    .artifacts
                    .iter()
                    .map(artifact_to_record)
                    .collect(),
                final_cost: completion.final_cost.as_ref().map(cost_to_facets),
                actual_spend: completion.final_cost.as_ref().map(|c| c.spend),
                summary_json: completion
                    .result
                    .as_ref()
                    .map(|result| json!({ "result": result })),
                budget_status: Some("released".to_string()),
                budget_metadata: completion
                    .continuation_request
                    .as_ref()
                    .map(|cr| json!({ "continuation_request": { "reason": cr.reason } })),
            },
        )?;
        self.events.publish_persisted_batch(&persisted);

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

    pub fn finalize_thread(&self, params: &ThreadFinalizeParams) -> Result<ThreadDetail> {
        let persisted = self.db.finalize_thread(
            &params.thread_id,
            &FinalizeThreadRecord {
                status: normalize_terminal_status(&params.status)?.to_string(),
                outcome_code: params.outcome_code.clone(),
                result_json: params.result.clone(),
                error_json: params.error.clone(),
                metadata: params.metadata.clone(),
                artifacts: params.artifacts.iter().map(artifact_to_record).collect(),
                final_cost: params.final_cost.as_ref().map(cost_to_facets),
                actual_spend: params.final_cost.as_ref().map(|c| c.spend),
                summary_json: params.summary_json.clone(),
                budget_status: Some("released".to_string()),
                budget_metadata: None,
            },
        )?;
        self.events.publish_persisted_batch(&persisted);

        self.get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {}", params.thread_id))
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        self.db.get_thread(thread_id)
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        self.db.get_thread_result(thread_id)
    }

    pub fn list_thread_artifacts(&self, thread_id: &str) -> Result<Vec<ThreadArtifactRecord>> {
        self.db.list_thread_artifacts(thread_id)
    }

    pub fn build_execute_result(&self, thread_id: &str) -> Result<Option<ExecuteResponseResult>> {
        let result = self.db.get_thread_result(thread_id)?;
        let artifacts = self.db.list_thread_artifacts(thread_id)?;
        Ok(result.map(|result| ExecuteResponseResult {
            outcome_code: result.outcome_code,
            result: result.result,
            error: result.error,
            artifacts,
        }))
    }

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
            status: "created".to_string(),
            item_ref: source.item_ref.clone(),
            executor_ref: source.executor_ref.clone(),
            launch_mode: source.launch_mode.clone(),
            current_site_id: source.current_site_id.clone(),
            origin_site_id: source.origin_site_id.clone(),
            upstream_thread_id: Some(source.thread_id.clone()),
            requested_by: source.requested_by.clone(),
            summary_json: None,
        };

        // Create successor, write continued edge, finalize source — all in one transaction
        let persisted = self.db.create_continuation(
            &successor_record,
            &source.thread_id,
            &source.chain_root_id,
            params.reason.as_deref(),
        )?;
        self.events.publish_persisted_batch(&persisted);

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

    pub fn publish_artifact(&self, params: &ArtifactPublishParams) -> Result<ThreadArtifactRecord> {
        let (artifact, persisted) = self.db.publish_artifact(
            &params.thread_id,
            &NewArtifactRecord {
                artifact_type: params.artifact_type.clone(),
                uri: params.uri.clone(),
                content_hash: params.content_hash.clone(),
                metadata: params.metadata.clone(),
            },
        )?;
        self.events.publish_persisted_batch(&persisted);
        Ok(artifact)
    }

    pub fn list_threads(&self, limit: usize) -> Result<Value> {
        Ok(json!({
            "threads": self.db.list_threads(limit)?,
            "next_cursor": null,
        }))
    }

    pub fn list_children(&self, thread_id: &str) -> Result<Vec<ThreadDetail>> {
        self.db.list_thread_children(thread_id)
    }

    pub fn get_chain(&self, thread_id: &str) -> Result<Option<ThreadChainResult>> {
        let Some(thread) = self.get_thread(thread_id)? else {
            return Ok(None);
        };

        Ok(Some(ThreadChainResult {
            threads: self.db.list_chain_threads(&thread.chain_root_id)?,
            edges: self.db.list_chain_edges(&thread.chain_root_id)?,
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

fn cost_to_facets(cost: &FinalCost) -> Vec<(String, String)> {
    let mut facets = vec![
        ("cost.turns".to_string(), cost.turns.to_string()),
        (
            "cost.input_tokens".to_string(),
            cost.input_tokens.to_string(),
        ),
        (
            "cost.output_tokens".to_string(),
            cost.output_tokens.to_string(),
        ),
        ("cost.spend".to_string(), cost.spend.to_string()),
    ];
    if let Some(provider) = &cost.provider {
        facets.push(("cost.provider".to_string(), provider.clone()));
    }
    if let Some(metadata) = &cost.metadata {
        if let Ok(s) = serde_json::to_string(metadata) {
            facets.push(("cost.metadata_json".to_string(), s));
        }
    }
    facets
}

fn new_thread_id() -> String {
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
pub fn resolve_root_execution(
    engine: &Engine,
    site_id: &str,
    project_path: impl AsRef<Path>,
    item_ref: &str,
    launch_mode: &str,
    parameters: Value,
    requested_by: Option<String>,
    caller_scopes: Vec<String>,
    validate_only: bool,
) -> Result<ResolvedExecutionRequest> {
    let project_path = project_path.as_ref().to_path_buf();

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
        .or_else(|| {
            engine
                .default_executor_id_for(&plan_ctx, &resolved.kind)
                .ok()
                .flatten()
        })
        .ok_or_else(|| {
            anyhow!(
                "no executor found for kind '{}' (item: {})",
                resolved.kind,
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

    crate::policy::enforce_trust(verified.trust_class, verified.resolved.source_space).map_err(
        |(_status, json_body)| {
            anyhow!(
                "trust policy denied: {}",
                json_body
                    .0
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("unknown")
            )
        },
    )?;

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
    spawned: rye_engine::dispatch::SpawnedExecution,
}

impl SpawnedItem {
    /// Block until subprocess completes.
    pub fn wait(self) -> ExecutionCompletion {
        self.spawned.wait()
    }
}

/// Run the engine pipeline: verify → build_plan → spawn.
/// Returns a handle with pid/pgid that the daemon can persist before calling wait().
pub fn spawn_item(
    engine: &Engine,
    resolved: &ResolvedExecutionRequest,
    thread_id: &str,
    chain_root_id: &str,
    mut extra_runtime_bindings: std::collections::HashMap<String, String>,
) -> Result<SpawnedItem> {
    if let Ok(socket_path) = std::env::var("RYEOSD_SOCKET_PATH") {
        extra_runtime_bindings
            .entry("RYEOSD_SOCKET_PATH".to_string())
            .or_insert(socket_path);
    }
    if let Ok(url) = std::env::var("RYEOSD_URL") {
        extra_runtime_bindings
            .entry("RYEOSD_URL".to_string())
            .or_insert(url);
    }
    let verified = engine
        .verify(&resolved.plan_context, resolved.resolved_item.clone())
        .map_err(|e| anyhow!("verification failed: {e}"))?;

    // Trust policy gate: reject untrusted items from user/system space
    crate::policy::enforce_trust(verified.trust_class, verified.resolved.source_space).map_err(
        |(_status, json_body)| {
            anyhow!(
                "trust policy denied: {}",
                json_body
                    .0
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("unknown")
            )
        },
    )?;

    let mut plan = engine
        .build_plan(
            &resolved.plan_context,
            &verified,
            &resolved.parameters,
            &resolved.plan_context.execution_hints,
        )
        .map_err(|e| anyhow!("plan build failed: {e}"))?;

    // Inject extra runtime bindings (e.g. vault env vars) into subprocess nodes
    if !extra_runtime_bindings.is_empty() {
        for node in &mut plan.nodes {
            if let rye_engine::contracts::PlanNode::DispatchSubprocess {
                runtime_bindings, ..
            } = node
            {
                runtime_bindings.extend(extra_runtime_bindings.clone());
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

    let spawned = engine
        .spawn_plan(&engine_ctx, &plan)
        .map_err(|e| anyhow!("spawn failed: {e}"))?;

    Ok(SpawnedItem {
        pid: spawned.pid,
        pgid: spawned.pgid,
        spawned,
    })
}

/// Map a canonical item kind to the daemon's thread kind for profiling.
/// Convention: thread kind = "{item_kind}_run" unless the kind profile
/// registry has a more specific mapping.
fn map_to_thread_kind(canonical_kind: &str) -> String {
    format!("{canonical_kind}_run")
}
