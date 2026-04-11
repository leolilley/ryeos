use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use directories::BaseDirs;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::bridge::{ExecutionArtifact, ExecutionCompletion, ExecutionRequest, FinalCost};
use crate::db::{
    Database, FinalizeThreadRecord, NewArtifactRecord, NewThreadRecord, RuntimeCostRecord,
    ThreadArtifactRecord, ThreadBudgetRecord, ThreadDetail, ThreadEdgeRecord, ThreadResultRecord,
};
use crate::kind_profiles::KindProfileRegistry;
use crate::services::event_store::EventStoreService;

const DIRECTIVE_EXECUTOR_REF: &str = "tool:rye/agent/threads/thread_directive";

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
    pub project_path: PathBuf,
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

    pub fn resolve_root_execution(
        &self,
        project_path: impl AsRef<Path>,
        item_ref: &str,
        launch_mode: &str,
        parameters: Value,
        requested_by: Option<String>,
        model: Option<String>,
        budget: Option<ExecuteBudgetRequest>,
    ) -> Result<ResolvedExecutionRequest> {
        let project_path = project_path.as_ref().to_path_buf();
        let resolved_item = resolve_item(&project_path, item_ref)?;

        Ok(ResolvedExecutionRequest {
            kind: resolved_item.kind,
            item_ref: item_ref.to_string(),
            executor_ref: resolved_item.executor_ref,
            launch_mode: launch_mode.to_string(),
            current_site_id: self.current_site_id.clone(),
            origin_site_id: self.current_site_id.clone(),
            requested_by,
            model,
            budget,
            parameters,
            project_path,
        })
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
        let terminal_status = normalize_terminal_status(&completion.status)?;
        let outcome_code = Some(if terminal_status == "completed" {
            "success".to_string()
        } else {
            completion.status.clone()
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
                    .map(|value| json!({ "continuation_request": value })),
            },
        )?;
        self.events.publish_persisted_batch(&persisted);

        let finalized = self.get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {thread_id}"))?;

        // Handle continuation request from runtime
        if completion.continuation_request.is_some() && terminal_status == "completed" {
            match self.request_continuation(&ThreadContinuationParams {
                thread_id: thread_id.to_string(),
                reason: completion
                    .continuation_request
                    .as_ref()
                    .and_then(|v| v.get("reason"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
            }) {
                Ok(_continuation) => {
                    // Source thread is now "continued", return updated state
                    return self.get_thread(thread_id)?
                        .ok_or_else(|| anyhow!("thread not found after continuation: {thread_id}"));
                }
                Err(err) => {
                    eprintln!("ryeosd: continuation request failed for {thread_id}: {err:#}");
                    // Fall through — thread stays in its finalized state
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

    pub fn build_execution_request(
        &self,
        thread: &ThreadDetail,
        project_path: impl AsRef<Path>,
        parameters: Value,
        continuation_from_id: Option<String>,
    ) -> ExecutionRequest {
        ExecutionRequest {
            thread_id: thread.thread_id.clone(),
            chain_root_id: thread.chain_root_id.clone(),
            kind: thread.kind.clone(),
            item_ref: thread.item_ref.clone(),
            executor_ref: thread.executor_ref.clone(),
            launch_mode: thread.launch_mode.clone(),
            project_path: project_path.as_ref().display().to_string(),
            parameters,
            requested_by: thread.requested_by.clone(),
            current_site_id: thread.current_site_id.clone(),
            origin_site_id: thread.origin_site_id.clone(),
            upstream_thread_id: thread.upstream_thread_id.clone(),
            continuation_from_id,
            model: thread.model.clone(),
            runtime: crate::bridge::RuntimeBridgeConfig {
                socket_path: String::new(),
            },
        }
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

struct ResolvedItem {
    kind: String,
    executor_ref: String,
}

fn resolve_item(project_path: &Path, item_ref: &str) -> Result<ResolvedItem> {
    let (kind, bare_id) = parse_canonical_ref(item_ref)?;

    match kind.as_str() {
        "directive" => {
            find_item_path(project_path, &kind, &bare_id, &[".md"])?;
            Ok(ResolvedItem {
                kind: "directive_run".to_string(),
                executor_ref: DIRECTIVE_EXECUTOR_REF.to_string(),
            })
        }
        "tool" => resolve_tool_item(project_path, &bare_id),
        "knowledge" => bail!("knowledge items are not executable: {item_ref}"),
        other => bail!("unsupported canonical kind: {other}"),
    }
}

fn resolve_tool_item(project_path: &Path, bare_id: &str) -> Result<ResolvedItem> {
    let path = find_item_path(
        project_path,
        "tool",
        bare_id,
        &[".py", ".yaml", ".yml", ".sh", ".js", ".ts"],
    )?;
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read tool metadata from {}", path.display()))?;

    let (tool_type, executor_id) = match path.extension().and_then(|value| value.to_str()) {
        Some("py") | Some("sh") | Some("js") | Some("ts") => (
            parse_python_assignment(&contents, "__tool_type__"),
            parse_python_assignment(&contents, "__executor_id__"),
        ),
        Some("yaml") | Some("yml") => parse_yaml_tool_metadata(&contents)?,
        _ => (None, None),
    };

    let executor_id =
        executor_id.ok_or_else(|| anyhow!("tool is missing executor_id: {bare_id}"))?;
    let kind = if tool_type.as_deref() == Some("state_graph") {
        "graph_run"
    } else {
        "tool_run"
    };

    Ok(ResolvedItem {
        kind: kind.to_string(),
        executor_ref: format!("tool:{executor_id}"),
    })
}

fn parse_canonical_ref(item_ref: &str) -> Result<(String, String)> {
    let (kind, bare_id) = item_ref
        .split_once(':')
        .ok_or_else(|| anyhow!("canonical item_ref required: {item_ref}"))?;
    if bare_id.is_empty() {
        bail!("canonical item_ref missing bare ID: {item_ref}");
    }
    Ok((kind.to_string(), bare_id.to_string()))
}

fn parse_python_assignment(contents: &str, key: &str) -> Option<String> {
    for line in contents.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = rest.trim();
        let quote = value.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }

        let mut parsed = String::new();
        let mut escape = false;
        for ch in value[1..].chars() {
            if escape {
                parsed.push(ch);
                escape = false;
                continue;
            }
            if ch == '\\' {
                escape = true;
                continue;
            }
            if ch == quote {
                return Some(parsed);
            }
            parsed.push(ch);
        }
    }
    None
}

fn parse_yaml_tool_metadata(contents: &str) -> Result<(Option<String>, Option<String>)> {
    #[derive(Deserialize)]
    struct ToolMetadata {
        tool_type: Option<String>,
        executor_id: Option<String>,
    }

    let metadata: ToolMetadata =
        serde_yaml::from_str(contents).context("failed to parse tool yaml")?;
    Ok((metadata.tool_type, metadata.executor_id))
}

fn find_item_path(
    project_path: &Path,
    kind: &str,
    bare_id: &str,
    extensions: &[&str],
) -> Result<PathBuf> {
    for base in search_roots(project_path, kind)? {
        for extension in extensions {
            let candidate = base.join(format!("{bare_id}{extension}"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    bail!("{kind} item not found: {bare_id}")
}

fn search_roots(project_path: &Path, kind: &str) -> Result<Vec<PathBuf>> {
    let folder = match kind {
        "tool" => "tools",
        "directive" => "directives",
        "knowledge" => "knowledge",
        other => bail!("unsupported item kind: {other}"),
    };

    let mut roots = vec![project_path.join(".ai").join(folder)];

    let user_root = env::var_os("USER_SPACE")
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
        .ok_or_else(|| anyhow!("unable to determine user space"))?;
    roots.push(user_root.join(".ai").join(folder));

    for bundle_root in repo_bundle_roots() {
        roots.push(bundle_root.join(".ai").join(folder));
    }

    Ok(roots)
}

fn repo_bundle_roots() -> Vec<PathBuf> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    [
        repo_root.join("ryeos/bundles/standard/ryeos_std"),
        repo_root.join("ryeos/bundles/code/ryeos_code"),
        repo_root.join("ryeos/bundles/core/ryeos_core"),
        repo_root.join("ryeos/bundles/email/ryeos_email"),
        repo_root.join("ryeos/bundles/web/ryeos_web"),
    ]
    .into_iter()
    .filter(|path| path.join(".ai").is_dir())
    .collect()
}
