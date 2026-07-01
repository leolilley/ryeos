use std::env;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::env_contract::{DaemonRootEnv, EnvBinding, EnvSourceDetail, EnvSourceKind};
use crate::event_store_service::EventStoreService;
use crate::event_stream::ThreadEventHub;
use crate::kind_profiles::KindProfileRegistry;
use crate::state_store::{
    FinalizeThreadRecord, NewArtifactRecord, NewEventRecord, NewThreadRecord, PersistedEventRecord,
    StateStore, ThreadArtifactRecord, ThreadDetail, ThreadEdgeRecord, ThreadListItem,
    ThreadResultRecord,
};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    EffectivePrincipal, EngineContext, ExecutionArtifact, ExecutionCompletion, ExecutionHints,
    FinalCost, LaunchMode, PlanContext, Principal, ProjectContext, ResolvedItem, RuntimeEnvSource,
    ThreadTerminalStatus, TrustClass,
};
use ryeos_engine::engine::Engine;
use ryeos_state::UsageSubject;

pub struct ThreadLifecycleService {
    state_store: Arc<StateStore>,
    kind_profiles: Arc<KindProfileRegistry>,
    _events: Arc<EventStoreService>,
    /// Live broadcast hub. Lifecycle writes (create / start / finalize /
    /// continuation) persist through `state_store` and THEN publish the
    /// resulting records here, so SSE subscribers see the same persisted
    /// events — including terminal ones — live, not only via replay. This
    /// is the "persist then publish" invariant in one place rather than
    /// re-derived per call site (see `publish_records`).
    event_hub: Arc<ThreadEventHub>,
    current_site_id: String,
    scheduler_db: std::sync::RwLock<Option<Arc<ryeos_scheduler::db::SchedulerDb>>>,
    app_root: std::sync::RwLock<Option<std::path::PathBuf>>,
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
    #[serde(default)]
    pub usage_subject: Option<UsageSubject>,
    #[serde(default)]
    pub usage_subject_asserted_by: Option<String>,
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
    /// Process-group id. The UDS `runtime.attach_process` wire reports `pid`
    /// only (the runtime knows its pid, not its group), so this defaults to 0
    /// and is derived daemon-side via `process::pgid_of`. Direct in-process
    /// callers (the detached spawn path) set it explicitly.
    #[serde(default)]
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
    pub threads: Vec<ThreadView>,
    pub edges: Vec<ThreadEdgeRecord>,
}

/// Daemon-authored execution facts decorated onto a client-facing thread
/// projection. These are kind-policy values (mirroring what lifecycle
/// enforcement already consults), composed here in the lifecycle layer — the
/// persistence module (`state_store`) stays free of kind-policy vocabulary. A
/// client gates machine-continuation affordances on `supports_continuation` and
/// operator-input affordances on `supports_operator_followup`, rather than on a
/// self-declared surface flag.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ExecutionFacts {
    /// Whether this thread's kind can continue into a successor — chain-fold for
    /// conversation kinds, checkpoint resume (machine) for graph.
    pub supports_continuation: bool,
    /// Whether an OPERATOR follow-up (`threads.input`) can continue this kind.
    /// `false` for machine-only kinds (graph) even when `supports_continuation`
    /// is `true`, so a client gates the operator-input affordance on this rather
    /// than on `supports_continuation` alone.
    pub supports_operator_followup: bool,
}

/// A `ThreadDetail` projection decorated with daemon-authored
/// [`ExecutionFacts`]. The detail's fields serialize flat alongside an
/// `execution` object, so a client gates affordances on
/// `execution.{supports_continuation, supports_operator_followup}` (substrate
/// authority for the actual target thread) rather than a self-declared surface
/// flag.
#[derive(Debug, Serialize)]
pub struct ThreadView {
    #[serde(flatten)]
    pub thread: ThreadDetail,
    pub execution: ExecutionFacts,
}

/// A `ThreadListItem` projection decorated with daemon-authored
/// [`ExecutionFacts`] — the list-row analogue of [`ThreadView`].
#[derive(Debug, Serialize)]
pub struct ThreadListView {
    #[serde(flatten)]
    pub item: ThreadListItem,
    pub execution: ExecutionFacts,
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

/// Default auto-launch attempt budget for a machine continuation successor. The
/// reconciler/launcher reuses the runtime-db resume-attempts counter against
/// this ceiling so a poisoned successor can't relaunch forever.
pub const MAX_CONTINUATION_AUTO_ATTEMPTS: u32 = 3;

/// The chain-level ceiling: the maximum length of an AUTONOMOUS (machine)
/// continuation run. A directive cut off by its turn limit hands off to a
/// successor that folds and continues; without a bound, a task that never
/// completes would spawn an unbounded chain. At this many CONSECUTIVE machine
/// continuations the cut-off thread does not continue — the chain terminates.
/// An operator follow-up resets the count, so this never caps an operator
/// conversation, only a runaway autonomous one. This bound is what makes
/// autonomous machine continuation safe to be always-on (no env gate).
pub const MAX_CONTINUATION_CHAIN_DEPTH: u32 = 12;

/// Stable fingerprint of an OPERATOR follow-up request over its launch-significant
/// boundary inputs. Two `threads/input` calls with the same fingerprint are the
/// SAME logical request (a double-submit) and resolve to the same successor; a
/// different fingerprint onto an already-continued source is a conflict.
///
/// Covers the item ref, project, principal + scopes, launch mode, the predecessor
/// thread id, and the raw parameters — everything that distinguishes one launch
/// from another. The dedup must work BEFORE the runtime launches (a crash between
/// create and launch must still dedup), so this is persisted on the successor's
/// `thread_continued` edge, not re-derived from runtime-emitted input. Not
/// canonical JSON: identical requests serialize identically, which is all dedup
/// needs; scopes are sorted so ordering is not significant.
pub fn continuation_request_fingerprint(
    item_ref: &str,
    project_path: &str,
    principal_fingerprint: &str,
    scopes: &[String],
    launch_mode: &str,
    previous_thread_id: &str,
    parameters: &Value,
) -> String {
    use sha2::{Digest, Sha256};
    let mut scopes_sorted: Vec<&str> = scopes.iter().map(String::as_str).collect();
    scopes_sorted.sort_unstable();
    let canonical = json!({
        "item_ref": item_ref,
        "project_path": project_path,
        "principal": principal_fingerprint,
        "scopes": scopes_sorted,
        "launch_mode": launch_mode,
        "previous_thread_id": previous_thread_id,
        "parameters": parameters,
    });
    let bytes = serde_json::to_vec(&canonical).expect("fingerprint input is serializable");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    format!("sha256:{:x}", hasher.finalize())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadContinuationParams {
    pub thread_id: String,
    /// Optional free-form string from the runtime for logs only. Continuation is
    /// autonomous by construction — it carries no reason/gate/mode.
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

/// Outcome of an idempotent operator follow-up (see
/// [`ThreadLifecycleService::create_or_get_operator_continuation`]). The handler
/// returns the successor's real id in every non-conflict case, so a double-submit
/// never acknowledges a phantom id.
#[derive(Debug)]
pub enum OperatorContinuation {
    /// A new successor was created — the caller should launch it.
    Created(ThreadDetail),
    /// A same-fingerprint successor already exists (a duplicate submit) — return
    /// it; do not mint or launch a second.
    Existing(ThreadDetail),
    /// The source is already continued by a DIFFERENT request — refuse, pointing
    /// at the existing successor. Carries the successor's detail (not just its
    /// id) so the API decorates execution facts for the ACTUAL target object and
    /// fails loud if that row is missing/corrupt.
    Conflict(ThreadDetail),
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
    pub usage_subject: Option<UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
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
        event_hub: Arc<ThreadEventHub>,
    ) -> anyhow::Result<Self> {
        let hostname = env::var("HOSTNAME")
            .or_else(|_| hostname::get().map(|h| h.to_string_lossy().into_owned()))
            .map_err(|_| {
                anyhow::anyhow!(
                    "HOSTNAME env var not set and system hostname unavailable. \
                 Set HOSTNAME to this node's identity (e.g. hostname or unique site ID). \
                 This is used to construct the site_id for thread isolation."
                )
            })?;
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
            event_hub,
            current_site_id: format!("site:{hostname}"),
            scheduler_db: std::sync::RwLock::new(None),
            app_root: std::sync::RwLock::new(None),
        })
    }

    /// Publish persisted lifecycle records to live subscribers after the
    /// state-store write succeeded (persistence-first). Records are published
    /// in their original (`chain_seq`) order, each to its OWN thread's lane —
    /// a continuation touches both the source (`thread_continued`) and
    /// successor (`thread_created`) threads, and the firehose lane must see
    /// them in persisted order, so no per-thread regrouping. The whole slice
    /// publishes under one hub-lock acquire so a concurrent same-thread append
    /// cannot interleave a later `chain_seq` between two of these records.
    /// No-op when empty.
    fn publish_records(&self, records: &[PersistedEventRecord]) {
        self.event_hub.publish_ordered(records);
    }

    /// Wire the scheduler DB for thread completion tracking.
    /// Called once after construction, once the scheduler DB is available.
    pub fn set_scheduler_db(
        &self,
        db: Arc<ryeos_scheduler::db::SchedulerDb>,
        app_root: std::path::PathBuf,
    ) {
        *self.scheduler_db.write().unwrap() = Some(db);
        *self.app_root.write().unwrap() = Some(app_root);
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
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
        };

        let persisted = self.state_store.create_thread(&thread_record)?;
        self.publish_records(&persisted);

        self.get_thread(thread_id)?
            .ok_or_else(|| anyhow!("created thread missing from database: {thread_id}"))
    }

    /// Create a chain successor for `source_thread_id` with a pre-minted id and
    /// a launch-ready `request`, then return it. Mirrors
    /// `create_root_thread_with_id` but braids onto the source's chain: the
    /// successor inherits `chain_root_id` and links via `upstream_thread_id`,
    /// and the source records the handoff transactionally (a `thread_continued`
    /// event; the source is settled to `continued` only when it was still
    /// running). The operator follow-up path runs this so lineage is
    /// daemon-persisted at spawn, independent of whether the runtime rehydrates.
    #[tracing::instrument(
        level = "debug",
        name = "thread:create_continuation",
        skip(self, request),
        fields(
            thread_id = %successor_thread_id,
            source_thread_id = %source_thread_id,
            item_ref = %request.item_ref,
        )
    )]
    pub fn create_continuation_with_id(
        &self,
        successor_thread_id: &str,
        source_thread_id: &str,
        request: &ResolvedExecutionRequest,
        reason: Option<&str>,
    ) -> Result<ThreadDetail> {
        validate_kind(&request.kind, self.kind_profiles())?;
        validate_thread_id_format(successor_thread_id)?;

        let source = self
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("continuation source thread not found: {source_thread_id}"))?;

        let profile = self.kind_profiles().get(&source.kind);
        if !profile.is_some_and(|p| p.supports_continuation) {
            bail!("continuation is not supported for kind '{}'", source.kind);
        }

        let successor_record = NewThreadRecord {
            thread_id: successor_thread_id.to_string(),
            chain_root_id: source.chain_root_id.clone(),
            kind: request.kind.clone(),
            item_ref: request.item_ref.clone(),
            executor_ref: request.executor_ref.clone(),
            launch_mode: request.launch_mode.clone(),
            current_site_id: request.current_site_id.clone(),
            origin_site_id: request.origin_site_id.clone(),
            upstream_thread_id: Some(source.thread_id.clone()),
            requested_by: request.requested_by.clone(),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
        };

        let persisted = self.state_store.create_continuation(
            &successor_record,
            &source.thread_id,
            &source.chain_root_id,
            reason,
        )?;
        self.publish_records(&persisted);

        self.get_thread(successor_thread_id)?.ok_or_else(|| {
            anyhow!("created continuation successor missing from database: {successor_thread_id}")
        })
    }

    /// Operator follow-up continuation, idempotent by request fingerprint.
    ///
    /// The lifecycle-layer wrapper over [`StateStore::create_or_get_continuation`]:
    /// it enforces the source-kind `supports_continuation` policy (which the raw
    /// state-store method cannot, having no kind registry), drives the chain root
    /// from the source (never trusting the caller), publishes the persisted events
    /// for a freshly created successor, and returns handler-friendly
    /// `ThreadDetail`s. Same-fingerprint double-submits return `Existing`; a
    /// different fingerprint onto an already-continued source is `Conflict`.
    pub fn create_or_get_operator_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        reason: Option<&str>,
        request_fingerprint: &str,
        resume_context: &crate::launch_metadata::ResumeContext,
    ) -> Result<OperatorContinuation> {
        validate_kind(&successor.kind, self.kind_profiles())?;

        let source = self
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("continuation source thread not found: {source_thread_id}"))?;

        // OPERATOR follow-up gate: the source kind must accept operator input,
        // not merely support (machine) continuation. A graph self-continues by
        // machine only, so it is refused here even though it IS continuable.
        let profile = self.kind_profiles().get(&source.kind);
        if !profile.is_some_and(|p| p.supports_continuation && p.supports_operator_followup) {
            bail!(
                "operator follow-up is not supported for kind '{}'",
                source.kind
            );
        }

        // An operator follow-up braids onto a SETTLED turn (completed/failed) and
        // PRESERVES that status. Reject any other source here so this wrapper
        // cannot mint an illegal operator continuation: on a running source the
        // store would settle it to `continued` with an operator fingerprint, which
        // reconcile would then misclassify as a machine continuation.
        if source.status != ryeos_state::objects::ThreadStatus::Completed.as_str()
            && source.status != ryeos_state::objects::ThreadStatus::Failed.as_str()
        {
            bail!(
                "operator continuation requires a completed or failed source; \
                 thread '{source_thread_id}' is '{}'",
                source.status
            );
        }

        // An operator successor MUST carry a launch context — without it the row is
        // unrelaunchable (a stranded `created` thread reconcile/resubmit cannot
        // recover). The wrapper requires it; the raw state-store method's `Option`
        // is for the machine/test paths that seed metadata elsewhere.
        let outcome = self.state_store.create_or_get_continuation(
            successor,
            &source.thread_id,
            &source.chain_root_id,
            reason,
            request_fingerprint,
            Some(resume_context),
        )?;

        match outcome {
            crate::state_store::ContinuationOutcome::Created(events) => {
                self.publish_records(&events);
                let detail = self.get_thread(&successor.thread_id)?.ok_or_else(|| {
                    anyhow!(
                        "created continuation successor missing from database: {}",
                        successor.thread_id
                    )
                })?;
                Ok(OperatorContinuation::Created(detail))
            }
            crate::state_store::ContinuationOutcome::Existing {
                successor_thread_id,
            } => {
                let detail = self.get_thread(&successor_thread_id)?.ok_or_else(|| {
                    anyhow!("existing continuation successor missing: {successor_thread_id}")
                })?;
                Ok(OperatorContinuation::Existing(detail))
            }
            crate::state_store::ContinuationOutcome::Conflict {
                successor_thread_id,
            } => {
                let detail = self.get_thread(&successor_thread_id)?.ok_or_else(|| {
                    anyhow!("conflicting continuation successor missing: {successor_thread_id}")
                })?;
                Ok(OperatorContinuation::Conflict(detail))
            }
        }
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
            usage_subject: params.usage_subject.clone(),
            usage_subject_asserted_by: params.usage_subject_asserted_by.clone(),
        };

        let persisted = self.state_store.create_thread(&thread_record)?;
        self.publish_records(&persisted);

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
        let persisted = self.state_store.mark_thread_running(thread_id, None)?;
        self.publish_records(&persisted);
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

        let persisted = self.state_store.finalize_thread(
            thread_id,
            &FinalizeThreadRecord {
                status: terminal_status.to_string(),
                outcome_code: outcome_code.clone(),
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
        self.publish_records(&persisted);

        // Failure-gated stderr surfacing. A subprocess tool's real error
        // (traceback, `logger.error(...)`) rides in `error.stderr`; on the
        // scheduled path the only live signal operators see is the daemon's
        // tracing stream (deploy logs), so emit it there at ERROR level when
        // the run failed. Healthy runs stay silent — no stderr in `error`.
        if matches!(terminal_status, "failed" | "killed") {
            if let Some(stderr_tail) = completion
                .error
                .as_ref()
                .and_then(|e| e.get("stderr"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let exit_code = completion
                    .error
                    .as_ref()
                    .and_then(|e| e.get("exit_code"))
                    .and_then(Value::as_i64);
                let soft_failure = completion
                    .error
                    .as_ref()
                    .and_then(|e| e.get("soft_failure"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                tracing::error!(
                    thread_id,
                    outcome_code = outcome_code.as_deref().unwrap_or(""),
                    exit_code,
                    soft_failure,
                    stderr_tail = %stderr_tail,
                    "thread failed; tool stderr tail follows"
                );
            }
        }

        self.update_scheduler_fire_on_thread_terminal(
            thread_id,
            terminal_status,
            outcome_code.as_deref(),
            completion.result.as_ref(),
        );

        let finalized = self
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {thread_id}"))?;

        // A continuation is a *cut-off* handoff from a RUNNING source (the live
        // `request_continuation` UDS handler), never a terminal-completion edge:
        // a thread that completes — with or without a question — takes the
        // operator follow-up path. So finalize does NOT spawn a successor here.

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
        let persisted = self.state_store.finalize_thread(
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
        self.publish_records(&persisted);

        let terminal_status = normalize_terminal_status(&params.status)?;
        self.update_scheduler_fire_on_thread_terminal(
            &params.thread_id,
            terminal_status,
            params.outcome_code.as_deref(),
            params.result.as_ref(),
        );

        self.get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {}", params.thread_id))
    }

    fn update_scheduler_fire_on_thread_terminal(
        &self,
        thread_id: &str,
        terminal_status: &str,
        outcome_code: Option<&str>,
        result: Option<&Value>,
    ) {
        if let Ok(guard) = self.scheduler_db.read() {
            if let Some(ref db) = *guard {
                if let Ok(Some(fire)) = db.find_fire_by_thread(thread_id) {
                    let fire_status =
                        ryeos_scheduler::fire_status_for_thread_status(terminal_status);
                    let result_outcome = result.map(ryeos_scheduler::classify_result_payload);
                    let outcome_str = ryeos_scheduler::fire_outcome_for_terminal(
                        terminal_status,
                        result_outcome.as_ref(),
                        outcome_code,
                    );
                    let now = lillux::time::timestamp_millis();
                    let completed_fired_at = fire.fired_at.unwrap_or(now);

                    // Clone fields needed for JSONL before the move
                    let jsonl_fire_id = fire.fire_id.clone();
                    let jsonl_schedule_id = fire.schedule_id.clone();
                    let jsonl_scheduled_at = fire.scheduled_at;
                    let jsonl_trigger_reason = fire.trigger_reason.clone();
                    let jsonl_signer_fp = fire.signer_fingerprint.clone();

                    let updated = ryeos_scheduler::types::FireRecord {
                        status: fire_status.to_string(),
                        outcome: Some(outcome_str.to_string()),
                        fired_at: Some(completed_fired_at),
                        completed_at: Some(now),
                        ..fire
                    };
                    if let Err(e) = db.upsert_fire(&updated) {
                        tracing::warn!(
                            thread_id = %thread_id,
                            error = %e,
                            "scheduler: failed to update fire status on thread completion"
                        );
                    }

                    // Append completion to JSONL
                    if let Ok(dir_guard) = self.app_root.read() {
                        if let Some(ref sys_dir) = *dir_guard {
                            let entry = serde_json::json!({
                                "entry_type": fire_status,
                                "status": fire_status,
                                "fire_id": jsonl_fire_id,
                                "schedule_id": jsonl_schedule_id,
                                "scheduled_at": jsonl_scheduled_at,
                                "fired_at": completed_fired_at,
                                "thread_id": thread_id,
                                "completed_at": now,
                                "trigger_reason": jsonl_trigger_reason,
                                "outcome": outcome_str,
                                "signer_fingerprint": jsonl_signer_fp,
                            });
                            let fires_path = sys_dir
                                .join(ryeos_engine::AI_DIR)
                                .join("state")
                                .join("schedules")
                                .join(&jsonl_schedule_id)
                                .join("fires.jsonl");
                            if let Err(e) =
                                ryeos_scheduler::projection::append_jsonl_entry(&fires_path, &entry)
                            {
                                tracing::warn!(
                                    thread_id = %thread_id,
                                    error = %e,
                                    "scheduler: failed to append completion to JSONL"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        self.state_store.get_thread(thread_id)
    }

    /// Daemon-authored continuation authority for a thread kind: whether the
    /// kind can chain-fold a continuation successor. This is the SAME policy
    /// the lifecycle creation paths enforce (`create_continuation_with_id`,
    /// `request_continuation`, …) — surfaced read-only so a client can gate the
    /// continuation affordance up front instead of submitting and eating a
    /// refusal. Unknown kinds fail closed.
    pub fn supports_continuation_for_kind(&self, kind: &str) -> bool {
        self.kind_profiles()
            .get(kind)
            .is_some_and(|p| p.supports_continuation)
    }

    /// Whether an operator follow-up (`threads.input`) can continue this kind.
    /// Operator follow-up IMPLIES continuation — a kind that cannot continue at
    /// all cannot accept operator input — so the effective fact is
    /// `supports_continuation && supports_operator_followup`. That prevents a
    /// non-continuable kind (which defaults `supports_operator_followup: true`)
    /// from projecting the contradictory `{continuation: false, followup: true}`.
    /// A graph is `{continuation: true, followup: false}` (machine only). Unknown
    /// kinds fail closed.
    pub fn supports_operator_followup_for_kind(&self, kind: &str) -> bool {
        self.kind_profiles()
            .get(kind)
            .is_some_and(|p| p.supports_continuation && p.supports_operator_followup)
    }

    fn execution_facts(&self, kind: &str) -> ExecutionFacts {
        ExecutionFacts {
            supports_continuation: self.supports_continuation_for_kind(kind),
            supports_operator_followup: self.supports_operator_followup_for_kind(kind),
        }
    }

    /// Decorate a state-store thread projection with daemon-authored execution
    /// facts. Kept here (not in `StateStore`) because kind policy is a daemon
    /// concern, not state-store row data.
    fn decorate_thread(&self, thread: ThreadDetail) -> ThreadView {
        let execution = self.execution_facts(&thread.kind);
        ThreadView { thread, execution }
    }

    fn decorate_list_item(&self, item: ThreadListItem) -> ThreadListView {
        let execution = self.execution_facts(&item.kind);
        ThreadListView { item, execution }
    }

    /// `get_thread` for client-facing callers: the projection plus its
    /// daemon-authored execution facts.
    pub fn get_thread_view(&self, thread_id: &str) -> Result<Option<ThreadView>> {
        Ok(self.get_thread(thread_id)?.map(|t| self.decorate_thread(t)))
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

        // Kind policy (a daemon concern, not state-store): only a
        // `supports_continuation` kind can chain-fold. The running-source and
        // captured-`ResumeContext` invariants are enforced authoritatively under
        // the write permit + lock by `create_machine_continuation`.
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
            usage_subject: None,
            usage_subject_asserted_by: None,
        };

        // One atomic op under the state store's write permit + lock: re-verify the
        // source is still `running`, require its captured `ResumeContext`, create
        // the successor, seed the successor's launch identity (runtime-db writes
        // first), then settle the source `continued`. A race or seed failure
        // aborts with the source still running — never `continued` behind an
        // unlaunchable successor.
        let persisted = self.state_store.create_machine_continuation(
            &successor_record,
            &source.thread_id,
            &source.chain_root_id,
            params.reason.as_deref(),
        )?;
        self.publish_records(&persisted);

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

    /// Create a parent's follow-resume successor (created, NOT launched) and
    /// settle the parent `continued` in one atomic op, then publish the resulting
    /// events. The daemon follow keystone calls this to suspend the parent; the
    /// successor is launched later, on child-terminal, by the follow-resume path.
    /// Wraps the raw state-store op so event publishing stays a lifecycle concern.
    pub fn create_follow_resume_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
    ) -> Result<()> {
        let persisted = self.state_store.create_follow_resume_successor(
            successor,
            source_thread_id,
            chain_root_id,
        )?;
        self.publish_records(&persisted);
        Ok(())
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
        let (artifact, persisted) = self.state_store.publish_artifact(
            &params.thread_id,
            &NewArtifactRecord {
                artifact_type: params.artifact_type.clone(),
                uri: params.uri.clone(),
                content_hash: params.content_hash.clone(),
                metadata: params.metadata.clone(),
            },
        )?;
        self.publish_records(std::slice::from_ref(&persisted));
        Ok(artifact)
    }

    /// Append caller-supplied events to a running thread, then publish the
    /// persisted records live (persist-then-publish, same invariant as the
    /// lifecycle paths). Returns `None` when the thread is no longer running,
    /// so callers keep their own running-state error message. Routes seat
    /// braids and any other direct-append surface through one publish site.
    pub fn append_thread_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Option<Vec<PersistedEventRecord>>> {
        let persisted =
            self.state_store
                .append_events_if_thread_running(chain_root_id, thread_id, events)?;
        if let Some(records) = &persisted {
            self.publish_records(records);
        }
        Ok(persisted)
    }

    pub fn list_threads(&self, limit: usize) -> Result<Value> {
        let threads: Vec<ThreadListView> = self
            .state_store
            .list_threads(limit)?
            .into_iter()
            .map(|item| self.decorate_list_item(item))
            .collect();
        Ok(json!({ "threads": threads, "next_cursor": null }))
    }

    /// List threads with optional principal filtering, each decorated with
    /// daemon-authored execution facts. Typed counterpart to
    /// [`Self::list_threads_filtered`] for callers that consume the rows
    /// directly rather than the `threads.list` service envelope.
    ///
    /// When `filter_principal` is `Some(fp)`, only threads with
    /// `requested_by = fp` are returned. `None` returns all threads
    /// (internal callers that intentionally request an unfiltered view).
    pub fn list_thread_views_filtered(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
    ) -> Result<Vec<ThreadListView>> {
        Ok(self
            .state_store
            .list_threads_filtered(limit, filter_principal)?
            .into_iter()
            .map(|item| self.decorate_list_item(item))
            .collect())
    }

    /// `threads.list` service envelope: the decorated rows plus a cursor.
    pub fn list_threads_filtered(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
    ) -> Result<Value> {
        let threads = self.list_thread_views_filtered(limit, filter_principal)?;
        Ok(json!({ "threads": threads, "next_cursor": null }))
    }

    pub fn list_children(&self, thread_id: &str) -> Result<Vec<ThreadView>> {
        Ok(self
            .state_store
            .list_thread_children(thread_id)?
            .into_iter()
            .map(|thread| self.decorate_thread(thread))
            .collect())
    }

    pub fn get_chain(&self, thread_id: &str) -> Result<Option<ThreadChainResult>> {
        let Some(thread) = self.get_thread(thread_id)? else {
            return Ok(None);
        };

        let threads = self
            .state_store
            .list_chain_threads(&thread.chain_root_id)?
            .into_iter()
            .map(|thread| self.decorate_thread(thread))
            .collect();
        Ok(Some(ThreadChainResult {
            threads,
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
        bail!("thread_id suffix must have 5 dash-separated hex groups: got `{suffix}`");
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
    pub usage_subject: Option<UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
    pub caller_scopes: Vec<String>,
    pub validate_only: bool,
}

pub fn resolve_root_execution(
    params: ResolveRootExecutionParams<'_>,
) -> Result<ResolvedExecutionRequest> {
    let ResolveRootExecutionParams {
        engine,
        site_id,
        project_path,
        item_ref,
        launch_mode,
        parameters,
        requested_by,
        usage_subject,
        usage_subject_asserted_by,
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
                "root item `{}` is not directly executable because it has no root executor_id or declares `executor_id: null`; terminal executors end an executor chain. Define a wrapper tool with `executor_id: \"@subprocess\"` and a `config:` block, then execute the wrapper.",
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
        usage_subject,
        usage_subject_asserted_by,
        parameters,
        resolved_item: resolved,
        plan_context: plan_ctx,
    })
}

/// Kind-agnostic existence + trust gate for accepted/background launch.
///
/// Resolves the root item and verifies its trust, returning the resolved
/// item so the caller can enforce required caps and secrets from its
/// metadata before a thread_id is minted. Unlike [`resolve_root_execution`]
/// (the terminal-only resolver used by the tool subprocess path), this does
/// not demand `metadata.executor_id` or build a plan — which executor runs
/// (terminal subprocess vs runtime-registry runtime) is decided by
/// `dispatch::dispatch`. Route-specific pre-thread-creation checks
/// (terminal executor_id, direct-runtime caps) live in
/// `ryeos_executor::dispatch::preflight_root_dispatch`.
///
/// Fails fast — before any thread_id is minted — on invalid refs and trust
/// violations.
pub fn preflight_root_execution(params: ResolveRootExecutionParams<'_>) -> Result<ResolvedItem> {
    let ResolveRootExecutionParams {
        engine,
        site_id,
        project_path,
        item_ref,
        launch_mode,
        requested_by,
        caller_scopes,
        validate_only,
        ..
    } = params;
    let project_path = project_path.to_path_buf();

    let canonical_ref =
        CanonicalRef::parse(item_ref).map_err(|e| anyhow!("invalid item ref: {e}"))?;

    validate_launch_mode(launch_mode)?;

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: requested_by.unwrap_or_else(|| "fp:local".into()),
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

    let verified = engine
        .verify(&plan_ctx, resolved)
        .map_err(|e| anyhow!("verification failed: {e}"))?;

    Ok(verified.resolved)
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
    pub roots: DaemonRootEnv,
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
        roots,
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
    // roots (RYEOS_APP_ROOT) + declared secrets, then
    // layer the daemon callback infra (socket path, callback token,
    // thread id, project path) on top. Mirrors `execution::launch::
    // spawn_runtime`'s composition so engine-dispatched `bin:` items
    // (e.g. `bin:ryeos-core-tools`) see the same env contract as runtime-
    // binary spawns. Without this, `lillux::run`'s post-Part-B
    // `env_clear()` would leave the subprocess with only a handful of
    // RYEOSD_* vars without the operator app-root context.
    let secret_map: std::collections::BTreeMap<String, String> = vault_bindings
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Allocate native-resume env before final env composition so the
    // builder remains the final authority. Preserve the existing
    // FirstWins behavior: only the first native_resume subprocess gets
    // the checkpoint/resume bindings.
    let mut allocated_checkpoint_dir: Option<std::path::PathBuf> = None;
    let mut resume_env_for_first_native_resume: Option<Vec<EnvBinding>> = None;
    // This is the executor-chain spawn path: native_resume here is the
    // SUBPROCESS/tool form declared via the `native_resume` decorate handler
    // (`spec.execution.native_resume`). A runtime-registry kind (graph) declares
    // native_resume on its runtime YAML and resumes through the managed
    // `spawn_runtime` path instead (it needs a LaunchEnvelope this path can't
    // build), so it never reaches here.
    if let Some(ts_dir) = thread_state_dir {
        for node in &plan.nodes {
            if let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = node {
                if spec.execution.native_resume.is_some() {
                    let ckpt = ts_dir.join("checkpoints");
                    std::fs::create_dir_all(&ckpt).map_err(|e| {
                        anyhow!("failed to create checkpoint dir {}: {e}", ckpt.display())
                    })?;
                    let mut bindings = vec![EnvBinding::new(
                        "RYEOS_CHECKPOINT_DIR",
                        ckpt.display().to_string(),
                        EnvSourceDetail::DaemonResume,
                    )];
                    if is_resume {
                        bindings.push(EnvBinding::new(
                            "RYEOS_RESUME",
                            "1",
                            EnvSourceDetail::DaemonResume,
                        ));
                    }
                    allocated_checkpoint_dir = Some(ckpt);
                    resume_env_for_first_native_resume = Some(bindings);
                    break; // first DispatchSubprocess wins, mirrors FirstWins
                }
            }
        }
    }

    for node in &mut plan.nodes {
        if let ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. } = node {
            let mut builder = crate::env_contract::EnvContractBuilder::new()
                .with_base_allowlist(std::env::vars_os().map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                }))?
                .with_daemon_roots(roots.clone())?
                .with_bindings(
                    EnvSourceKind::DeclaredSecret,
                    secret_map.iter().map(|(k, v)| (k.clone(), v.clone())),
                )?;

            builder = builder.with_typed_bindings([
                EnvBinding::new(
                    "RYEOS_THREAD_ID",
                    thread_id.to_string(),
                    EnvSourceDetail::EnginePlanEnv,
                ),
                EnvBinding::new(
                    "RYEOS_CHAIN_ROOT_ID",
                    chain_root_id.to_string(),
                    EnvSourceDetail::EnginePlanEnv,
                ),
            ])?;

            let runtime_bindings = spec.env.iter().map(|(key, value)| {
                let source = match spec.env_sources.get(key).copied() {
                    Some(RuntimeEnvSource::EnginePlan) => EnvSourceDetail::EnginePlanEnv,
                    Some(RuntimeEnvSource::RuntimeInterpreter) => {
                        EnvSourceDetail::RuntimeInterpreter
                    }
                    Some(RuntimeEnvSource::RuntimePathMutation) => {
                        EnvSourceDetail::RuntimePathMutation
                    }
                    Some(RuntimeEnvSource::RuntimeDescriptor) | None => {
                        EnvSourceDetail::RuntimeDescriptor
                    }
                };
                EnvBinding::new(key.clone(), value.clone(), source)
            });
            builder = builder.with_typed_bindings(runtime_bindings)?;

            let daemon_callback_bindings = daemon_callback_env.iter().filter_map(|(key, value)| {
                if key == "RYEOS_APP_ROOT" {
                    None
                } else {
                    Some(EnvBinding::new(
                        key.clone(),
                        value.clone(),
                        EnvSourceDetail::DaemonCallback,
                    ))
                }
            });
            builder = builder.with_typed_bindings(daemon_callback_bindings)?;

            if spec.execution.native_resume.is_some() {
                if let Some(resume_bindings) = resume_env_for_first_native_resume.take() {
                    builder = builder.with_typed_bindings(resume_bindings)?;
                }
            }

            spec.env = builder.build().into_iter().collect();
            spec.env_sources.clear();
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
    // working-dir head. See `docs/future/native-resume-snapshot-pinning.md`.
    if launch_metadata.native_resume.is_some() {
        launch_metadata =
            launch_metadata.with_resume_context(crate::launch_metadata::ResumeContext {
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
                // Subprocess (`spawn_item`) path: the executor identity is the
                // tool's own `executor_ref`; there is no serving runtime, so
                // `runtime_ref` stays `None`. (Runtime-registry launches capture
                // both in `launch::run_claimed_thread_row`.)
                executor_ref: Some(resolved.executor_ref.clone()),
                runtime_ref: None,
                // V5.5 P2: subprocess terminator has no permissions
                // composition step, so resumed callbacks inherit the
                // same deny-all posture the original spawn had. Native
                // runtime spawns that DO have a permissions model go
                // through `launch::build_and_launch`, not `spawn_item`.
                effective_caps: Vec::new(),
            });
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
