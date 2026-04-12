use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use rye_engine::canonical_ref::CanonicalRef;
use rye_engine::contracts::{
    BudgetRequest, EffectivePrincipal, EngineContext, ExecutionArtifact,
    ExecutionCompletion, ExecutionHints, FinalCost, ItemMetadata, LaunchMode, PlanContext,
    Principal, ProjectContext, ResolvedItem, ThreadTerminalStatus,
};
use rye_engine::engine::Engine;
use crate::db::{
    Database, FinalizeThreadRecord, NewArtifactRecord, NewThreadRecord, RuntimeCostRecord,
    ThreadArtifactRecord, ThreadBudgetRecord, ThreadDetail, ThreadEdgeRecord, ThreadResultRecord,
};
use crate::kind_profiles::KindProfileRegistry;
use crate::services::event_store::EventStoreService;

#[derive(Debug, Clone)]
pub struct ThreadLifecycleService {
    db: Arc<Database>,
    events: Arc<EventStoreService>,
    current_site_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecuteBudgetRequest {
    pub max_spend: f64,
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
    #[serde(default)]
    pub model: Option<String>,
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
    pub requested_by: Option<String>,
    pub model: Option<String>,
    pub budget: Option<ExecuteBudgetRequest>,
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
                model: request.model.clone(),
                summary_json: None,
            },
            Some(&ThreadBudgetRecord {
                budget_parent_id: None,
                reserved_spend: 0.0,
                actual_spend: 0.0,
                status: "open".to_string(),
                metadata: request
                    .budget
                    .as_ref()
                    .map(|budget| json!({ "max_spend": budget.max_spend })),
            }),
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
                model: params.model.clone(),
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
                final_cost: completion.final_cost.as_ref().map(cost_to_record),
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

        let finalized = self.get_thread(thread_id)?
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
                        return self.get_thread(thread_id)?
                            .ok_or_else(|| anyhow!("thread not found after continuation: {thread_id}"));
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
                final_cost: params.final_cost.as_ref().map(cost_to_record),
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
            bail!(
                "continuation is not supported for kind '{}'",
                source.kind
            );
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
            model: source.model.clone(),
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

        let successor = self.get_thread(&successor_id)?.ok_or_else(|| {
            anyhow!("successor thread missing after creation: {successor_id}")
        })?;

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

fn cost_to_record(cost: &FinalCost) -> RuntimeCostRecord {
    RuntimeCostRecord {
        provider: cost.provider.clone(),
        turns: cost.turns,
        input_tokens: cost.input_tokens,
        output_tokens: cost.output_tokens,
        spend: cost.spend,
        metadata: cost.metadata.clone(),
    }
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

/// Resolve a canonical item ref through the native engine.
pub fn resolve_root_execution(
    engine: &Engine,
    site_id: &str,
    project_path: impl AsRef<Path>,
    item_ref: &str,
    launch_mode: &str,
    parameters: Value,
    requested_by: Option<String>,
    caller_scopes: Vec<String>,
    model: Option<String>,
    budget: Option<ExecuteBudgetRequest>,
) -> Result<ResolvedExecutionRequest> {
    let project_path = project_path.as_ref().to_path_buf();

    let canonical_ref = CanonicalRef::parse(item_ref)
        .map_err(|e| anyhow!("invalid item ref: {e}"))?;

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: requested_by.clone().unwrap_or_else(|| "fp:local".into()),
            scopes: caller_scopes,
        }),
        project_context: ProjectContext::LocalPath { path: project_path },
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        execution_hints: ExecutionHints::default(),
        validate_only: false,
    };

    let resolved = engine.resolve(&plan_ctx, &canonical_ref)
        .map_err(|e| anyhow!("resolution failed: {e}"))?;

    let thread_kind = map_to_thread_kind(&resolved.kind, &resolved.metadata);

    let executor_ref = resolved.metadata.executor_id.clone()
        .or_else(|| engine.kinds.default_executor_id(&resolved.kind).map(String::from))
        .unwrap_or_default();

    Ok(ResolvedExecutionRequest {
        kind: thread_kind,
        item_ref: item_ref.to_string(),
        executor_ref,
        launch_mode: launch_mode.to_string(),
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        requested_by,
        model,
        budget,
        parameters,
        resolved_item: resolved,
        plan_context: plan_ctx,
    })
}

/// Run the full native engine pipeline: verify → build_plan → execute_plan.
pub fn execute_native(
    engine: &Engine,
    resolved: &ResolvedExecutionRequest,
    thread_id: &str,
    chain_root_id: &str,
) -> Result<ExecutionCompletion> {
    let verified = engine.verify(&resolved.plan_context, resolved.resolved_item.clone())
        .map_err(|e| anyhow!("verification failed: {e}"))?;

    let plan = engine.build_plan(
        &resolved.plan_context,
        &verified,
        &resolved.parameters,
        &resolved.plan_context.execution_hints,
    ).map_err(|e| anyhow!("plan build failed: {e}"))?;

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
        budget: resolved.budget.as_ref().map(|b| BudgetRequest {
            max_spend: b.max_spend,
        }),
    };

    engine.execute_plan(&engine_ctx, plan)
        .map_err(|e| anyhow!("execution failed: {e}"))
}

/// Map a canonical item kind to the daemon's thread kind for profiling.
fn map_to_thread_kind(canonical_kind: &str, metadata: &ItemMetadata) -> String {
    match canonical_kind {
        "directive" => "directive_run".to_string(),
        "graph" => "graph_run".to_string(),
        "tool" => {
            if metadata.extra.get("tool_type").and_then(|v| v.as_str()) == Some("state_graph") {
                "graph_run".to_string()
            } else {
                "tool_run".to_string()
            }
        }
        other => format!("{other}_run"),
    }
}


