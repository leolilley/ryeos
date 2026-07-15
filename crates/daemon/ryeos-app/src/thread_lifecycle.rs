use std::collections::HashMap;
use std::env;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::env_contract::{DaemonRootEnv, EnvBinding, EnvSourceDetail, EnvSourceKind};
use crate::event_store_service::EventStoreService;
use crate::event_stream::ThreadEventHub;
use crate::kind_profiles::KindProfileRegistry;
use crate::state_store::{
    ChildLineageAppendOutcome,
    FinalizeCreatedUnattachedOutcome as StoreFinalizeCreatedUnattachedOutcome,
    FinalizeIfNonterminalOutcome as StoreFinalizeIfNonterminalOutcome, FinalizeThreadRecord,
    NewArtifactRecord, NewEventRecord, NewThreadRecord, PersistedEventRecord, StateStore,
    ThreadArtifactRecord, ThreadDetail, ThreadEdgeRecord, ThreadListItem, ThreadResultRecord,
};
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::{
    DelegatedPrincipal, EffectivePrincipal, EngineContext, ExecutionArtifact, ExecutionCompletion,
    ExecutionHints, ExecutionPlan, FinalCost, ItemMetadata, ItemSpace, LaunchMode, PinnedVersion,
    PlanContext, Principal, ProjectContext, ResolvedItem, ResolvedSourceFormat, RuntimeEnvSource,
    ShadowedCandidate, SignatureEnvelope, SignatureHeader, SignerFingerprint,
    ThreadTerminalStatus, TrustClass, VerifiedItem,
};
use ryeos_engine::engine::Engine;
use ryeos_engine::history_policy::{
    NodeHistoryPolicyProvenance, PolicyProvenance, ResolvedNodeThreadHistoryPolicy,
    ResolvedThreadHistoryPolicy,
};
use ryeos_engine::resolution::TrustClass as ResolutionTrustClass;
use ryeos_state::UsageSubject;

/// Re-export so daemon crates that depend only on `ryeos-app` (e.g. `ryeos-ui`)
/// can name the watch sort without a direct `ryeos-state` dependency.
pub use ryeos_state::queries::{ThreadListFilter, ThreadSort};

mod validation;

use validation::{
    normalize_terminal_status, validate_kind, validate_launch_mode, validate_thread_id_format,
};

pub struct ThreadLifecycleService {
    state_store: Arc<StateStore>,
    engine: Arc<Engine>,
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
    scheduler_runtime_gate: std::sync::RwLock<Option<Arc<tokio::sync::RwLock<()>>>>,
    app_root: std::sync::RwLock<Option<std::path::PathBuf>>,
    /// Operator live-input staging, shared with `AppState`. Wired once after
    /// construction (`set_live_input_queue`) so the existing constructor call
    /// sites stay unchanged. Finalization closes the per-thread entry after the
    /// terminal state write so no queued input survives a terminal status and
    /// late enqueues are refused (see `live_input_queue`). Absent in tests
    /// that don't wire it — the persist-path running-guard still prevents any
    /// `cognition_in` append after terminal regardless.
    live_input: std::sync::RwLock<Option<Arc<crate::live_input_queue::LiveInputQueue>>>,
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
    pub project_root: Option<PathBuf>,
    #[serde(default)]
    pub usage_subject: Option<UsageSubject>,
    #[serde(default)]
    pub usage_subject_asserted_by: Option<String>,
    #[serde(default)]
    pub captured_history_policy: Option<ryeos_state::objects::CapturedThreadHistoryPolicy>,
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
    /// and is derived daemon-side while capturing the live process identity.
    /// Direct in-process callers (the detached spawn path) set it explicitly.
    #[serde(default)]
    pub pgid: i64,
    /// Daemon-captured, PID-reuse-safe identity. Wire callers omit this and the
    /// UDS boundary derives it from the live process before attachment.
    #[serde(default)]
    pub process_identity: Option<crate::process::ExecutionProcessIdentity>,
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

/// Which side of a graph `follow:` relationship a thread sits on. Serialized as
/// the `follow.role` wire string a client branches on.
pub mod follow_role {
    /// This thread issued a follow and settled `continued`, suspended until its
    /// followed child chain reaches terminal.
    pub const SUSPENDED_PARENT: &str = "suspended_parent";
    /// This thread is the suspended parent's resume successor — created (and
    /// later launched) to consume the child's result and continue the parent.
    pub const RESUME_SUCCESSOR: &str = "resume_successor";
}

/// The computed, display-facing lineage state a view tones and labels off,
/// serialized as `follow.display_state`. Coarser than [`follow_role`]: it drops
/// the parent/successor mechanics into the operator-legible question "is this
/// thread WAITING on a child, or RESUMING from one" — so a suspended follow
/// parent reads distinctly from a stalled segment-cut `continued` thread (which
/// carries no follow fact at all).
pub mod follow_display_state {
    /// A suspended parent awaiting its followed child chain
    /// ([`follow_role::SUSPENDED_PARENT`]).
    pub const SUSPENDED: &str = "suspended";
    /// A resume successor has been created and is waiting for the followed
    /// child chain to finish before it can consume the result.
    pub const RESUME_QUEUED: &str = "resume_queued";
    /// A resume successor consuming (or having consumed) the child's result
    /// ([`follow_role::RESUME_SUCCESSOR`]).
    pub const RESUMED: &str = "resumed";
}

/// Instance-derived follow-lineage fact decorated onto a thread projection when
/// the thread participates in a graph `follow:` relationship. Distinct from
/// [`ExecutionFacts`] (which are KIND-derived policy): this is per-thread INSTANCE
/// state, sourced live from the follow waiter (`runtime_db`) and, for terminal
/// history that outlives the waiter, from the projected `graph_follow_resume`
/// continuation edge (CAS is truth). Absent — the whole `follow` field is omitted
/// — for non-follow threads, so they pay nothing.
#[derive(Debug, Clone, Serialize)]
pub struct FollowFact {
    /// `suspended_parent` | `resume_successor` (see [`follow_role`]).
    pub role: &'static str,
    /// Computed display state (`suspended` | `resumed`, see
    /// [`follow_display_state`]): the coarse, tone-friendly lineage state a view
    /// labels/tones off so a suspended parent reads distinctly from a stalled
    /// `continued` thread. Always present alongside `role`.
    pub display_state: &'static str,
    /// Live waiter phase (`waiting` | `ready` | `resuming`); present for
    /// `suspended_parent` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// The graph node id that issued the follow. Absent only on a waiter-cleared
    /// resume successor recognized from the projection edge alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow_node: Option<String>,
    /// The followed child chain's head thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_thread_id: Option<String>,
    /// The followed child chain's root id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_chain_root_id: Option<String>,
    /// The child chain's terminal status once known; `null` while the child is
    /// still running. Always present (nullable) for a waiter-sourced fact.
    pub child_terminal_status: Option<String>,
    /// The parent's resume-successor thread id (the thread that consumes the
    /// child's result).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_successor_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cohort: Option<FollowCohortProgress>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FollowCohortProgress {
    pub done: u32,
    pub expected: u32,
}

impl FollowFact {
    /// A suspended parent (found by its own thread id in the live waiter).
    fn suspended_parent(w: &crate::runtime_db::FollowWaiter) -> Self {
        let child = w.children.first();
        Self {
            role: follow_role::SUSPENDED_PARENT,
            display_state: follow_display_state::SUSPENDED,
            phase: Some(w.phase.clone()),
            follow_node: Some(w.follow_node.clone()),
            child_thread_id: child.map(|c| c.child_thread_id.clone()),
            child_chain_root_id: child.map(|c| c.child_chain_root_id.clone()),
            child_terminal_status: child.and_then(|c| c.terminal_status.clone()),
            parent_successor_thread_id: w.parent_successor_thread_id.clone(),
            cohort: w.fanout.then(|| FollowCohortProgress {
                done: w
                    .children
                    .iter()
                    .filter(|c| c.terminal_status.is_some())
                    .count() as u32,
                expected: w.expected_children,
            }),
        }
    }

    /// A resume successor recognized from the still-live waiter.
    fn resume_successor_live(w: &crate::runtime_db::FollowWaiter) -> Self {
        let child = w.children.first();
        Self {
            role: follow_role::RESUME_SUCCESSOR,
            display_state: if w.children.iter().all(|c| c.terminal_status.is_some()) {
                follow_display_state::RESUMED
            } else {
                follow_display_state::RESUME_QUEUED
            },
            phase: None,
            follow_node: Some(w.follow_node.clone()),
            child_thread_id: child.map(|c| c.child_thread_id.clone()),
            child_chain_root_id: child.map(|c| c.child_chain_root_id.clone()),
            child_terminal_status: child.and_then(|c| c.terminal_status.clone()),
            parent_successor_thread_id: w.parent_successor_thread_id.clone(),
            cohort: w.fanout.then(|| FollowCohortProgress {
                done: w
                    .children
                    .iter()
                    .filter(|c| c.terminal_status.is_some())
                    .count() as u32,
                expected: w.expected_children,
            }),
        }
    }

    /// The list-path equivalent of [`Self::suspended_parent`], sourced from a
    /// bounded waiter summary that never loads child terminal envelopes.
    fn suspended_parent_summary(w: &crate::runtime_db::FollowWaiterSummary) -> Self {
        Self {
            role: follow_role::SUSPENDED_PARENT,
            display_state: follow_display_state::SUSPENDED,
            phase: Some(w.phase.clone()),
            follow_node: Some(w.follow_node.clone()),
            child_thread_id: w.first_child_thread_id.clone(),
            child_chain_root_id: w.first_child_chain_root_id.clone(),
            child_terminal_status: w.first_child_terminal_status.clone(),
            parent_successor_thread_id: w.parent_successor_thread_id.clone(),
            cohort: w.fanout.then(|| FollowCohortProgress {
                done: w.terminal_child_count,
                expected: w.expected_children,
            }),
        }
    }

    /// The list-path equivalent of [`Self::resume_successor_live`], sourced
    /// from the same envelope-free bounded waiter summary.
    fn resume_successor_live_summary(w: &crate::runtime_db::FollowWaiterSummary) -> Self {
        Self {
            role: follow_role::RESUME_SUCCESSOR,
            display_state: if w.all_children_terminal() {
                follow_display_state::RESUMED
            } else {
                follow_display_state::RESUME_QUEUED
            },
            phase: None,
            follow_node: Some(w.follow_node.clone()),
            child_thread_id: w.first_child_thread_id.clone(),
            child_chain_root_id: w.first_child_chain_root_id.clone(),
            child_terminal_status: w.first_child_terminal_status.clone(),
            parent_successor_thread_id: w.parent_successor_thread_id.clone(),
            cohort: w.fanout.then(|| FollowCohortProgress {
                done: w.terminal_child_count,
                expected: w.expected_children,
            }),
        }
    }

    /// A resume successor recognized after the waiter is cleared, from the
    /// predecessor's projected `graph_follow_resume` continuation edge. The child
    /// identities did not survive the waiter, so only the lineage role and this
    /// thread's own identity are known.
    fn resume_successor_durable(successor_thread_id: &str) -> Self {
        Self {
            role: follow_role::RESUME_SUCCESSOR,
            display_state: follow_display_state::RESUMED,
            phase: None,
            follow_node: None,
            child_thread_id: None,
            child_chain_root_id: None,
            child_terminal_status: None,
            parent_successor_thread_id: Some(successor_thread_id.to_string()),
            cohort: None,
        }
    }
}

/// A `ThreadDetail` projection decorated with daemon-authored [`ExecutionFacts`]
/// (kind-policy) plus per-instance follow lineage ([`FollowFact`]) and the live
/// operator-input staging depth. The detail's fields serialize flat alongside an
/// `execution` object, so a client gates affordances on
/// `execution.{supports_continuation, supports_operator_followup}` (substrate
/// authority for the actual target thread) rather than a self-declared surface
/// flag; `follow` names the graph follow lineage when present; `pending` is the
/// count of operator inputs staged for delivery (omitted when zero).
#[derive(Debug, Serialize)]
pub struct ThreadView {
    #[serde(flatten)]
    pub thread: ThreadDetail,
    pub execution: ExecutionFacts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<FollowFact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectSummary>,
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub pending: usize,
}

/// A `ThreadListItem` projection decorated with daemon-authored
/// [`ExecutionFacts`], follow lineage, and staged-input depth — the list-row
/// analogue of [`ThreadView`].
#[derive(Debug, Serialize)]
pub struct ThreadListView {
    #[serde(flatten)]
    pub item: ThreadListItem,
    pub execution: ExecutionFacts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub follow: Option<FollowFact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectSummary>,
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub pending: usize,
    /// Operator-stamped `(key, value)` facets (cohort/fleet tags via
    /// `threads.set_facet`), so a client view can column on `facets.<key>`
    /// (e.g. `facets.game`, `facets.fleet`). Empty for untagged threads.
    #[serde(skip_serializing_if = "is_empty_str_map")]
    pub facets: std::collections::BTreeMap<String, String>,
    /// Live execution head for a graph thread: `{node, step}` from its latest
    /// `graph_step_started`. `None` for a non-graph or not-yet-started thread.
    /// Lets a fleet overview show "where is each thread now" without a trace
    /// replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_node: Option<CurrentNode>,
}

/// A dashboard-decorated thread row plus its structural position in one
/// execution closure. `tree` is deliberately separate from thread/follow data:
/// it describes durable ancestry, while `follow` describes live/resume state.
#[derive(Debug, Serialize)]
pub struct ExecutionTreeView {
    #[serde(flatten)]
    pub thread: ThreadListView,
    pub tree: ExecutionTreePosition,
}

#[derive(Debug, Serialize)]
pub struct ExecutionTreePosition {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    pub relation: String,
    pub depth: usize,
    pub has_children: bool,
}

#[derive(Debug, Serialize)]
pub struct ExecutionTreeResult {
    pub root_thread_id: Option<String>,
    pub threads: Vec<ExecutionTreeView>,
    pub truncated: bool,
}

/// A graph thread's current node/step (see [`ThreadListView::current_node`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentNode {
    pub node: String,
    pub step: u32,
}

/// Project context attached to a thread from its launch metadata. This is
/// display/scoping metadata for the RyeOS UI, not an authorization boundary.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub path: String,
    pub name: String,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

fn is_empty_str_map(m: &std::collections::BTreeMap<String, String>) -> bool {
    m.is_empty()
}

fn project_summary(path: &Path) -> ProjectSummary {
    ProjectSummary {
        path: path.display().to_string(),
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_else(|| path.to_str().unwrap_or("project"))
            .to_string(),
    }
}

fn local_project_root(context: &PlanContext) -> Option<PathBuf> {
    let ProjectContext::LocalPath { path } = &context.project_context else {
        return None;
    };
    Some(path.clone())
}

fn sort_thread_list_items(items: &mut [ThreadListItem], sort: ryeos_state::queries::ThreadSort) {
    match sort {
        ryeos_state::queries::ThreadSort::Default => {
            items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        }
        ryeos_state::queries::ThreadSort::Newest | ryeos_state::queries::ThreadSort::Watch => {
            items.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        }
    }
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

/// Lifecycle-layer result of a conditional pre-launch cleanup. `Finalized` and
/// `AlreadyTerminal` are settled outcomes; `NotCurrent` means another owner
/// advanced or claimed the child, so the caller must not overwrite it.
#[derive(Debug)]
pub enum FinalizeCreatedUnattachedOutcome {
    Finalized(ThreadDetail),
    AlreadyTerminal(ThreadDetail),
    NotCurrent {
        thread: ThreadDetail,
        process_attached: bool,
        launch_claimed: bool,
    },
}

impl FinalizeCreatedUnattachedOutcome {
    pub fn is_settled(&self) -> bool {
        matches!(self, Self::Finalized(_) | Self::AlreadyTerminal(_))
    }
}

/// Lifecycle-layer result of an atomic finalize-if-live transition.
#[derive(Debug)]
pub enum FinalizeIfNonterminalOutcome {
    Finalized(ThreadDetail),
    AlreadyTerminal { status: String },
    PreservedForShutdown,
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
    ref_bindings: &BTreeMap<String, String>,
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
        "ref_bindings": ref_bindings,
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
    /// Exact terminal payload the runtime will emit after this atomic handoff.
    pub completion: ryeos_runtime::TerminalCompletion,
}

fn validate_continued_completion(completion: &ryeos_runtime::TerminalCompletion) -> Result<()> {
    if completion.status != ThreadTerminalStatus::Continued {
        bail!(
            "continuation completion status must be `{}`, received `{}`",
            ThreadTerminalStatus::Continued.as_str(),
            completion.status.as_str()
        );
    }
    if completion.error.is_some() {
        bail!("continued completion must not carry a terminal error");
    }
    if let Some(outcome_code) = completion.outcome_code.as_deref() {
        if outcome_code != ThreadTerminalStatus::Continued.as_str() {
            bail!(
                "continued completion outcome_code must be `{}`, received `{outcome_code}`",
                ThreadTerminalStatus::Continued.as_str()
            );
        }
    }

    let runtime_cost = completion
        .cost
        .as_ref()
        .map(|raw| serde_json::from_value::<ryeos_runtime::envelope::RuntimeCost>(raw.clone()))
        .transpose()
        .context("decode continued runtime cost")?;
    if let Some(cost) = runtime_cost.as_ref() {
        cost.validate().context("validate continued runtime cost")?;
    }
    Ok(())
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
    pub ref_bindings: BTreeMap<String, String>,
    /// The engine's resolved item — carried through for verify/build_plan/execute.
    pub resolved_item: ResolvedItem,
    /// Digest of the verified signature-stripped root bytes supplied to the
    /// runtime. Callback capabilities bind hook identities to this exact value.
    pub root_raw_content_digest: String,
    /// The PlanContext used during resolution — reused for verify/build_plan.
    pub plan_context: PlanContext,
    /// Immutable admission contract for a new chain root. It binds the exact
    /// verified subject bytes and provenance to the exact captured history
    /// policy persisted by root creation. `None` is valid only for an existing
    /// row or continuation, which inherits its authoritative root contract.
    pub root_admission: Option<RootExecutionAdmission>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

fn require_exact_object_keys(
    value: &Value,
    expected: &[&str],
    label: &str,
) -> std::result::Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{label} must be an object"))?;
    let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = expected.to_vec();
    expected.sort_unstable();
    if actual != expected {
        return Err(format!(
            "{label} must contain exactly [{}], got [{}]",
            expected.join(", "),
            actual.join(", ")
        ));
    }
    Ok(())
}

fn validate_exact_resolution_wire(value: &Value) -> std::result::Result<(), String> {
    require_exact_object_keys(
        value,
        &[
            "root",
            "ancestors",
            "references_edges",
            "referenced_items",
            "step_outputs",
            "effective_trust_class",
            "composed",
        ],
        "sealed resolution",
    )?;
    let validate_ancestor = |ancestor: &Value, label: &str| -> std::result::Result<(), String> {
        require_exact_object_keys(
            ancestor,
            &[
                "requested_id",
                "resolved_ref",
                "source_path",
                "trust_class",
                "alias_resolution",
                "added_by",
                "raw_content",
                "raw_content_digest",
            ],
            label,
        )?;
        if let Some(alias) = ancestor
            .get("alias_resolution")
            .filter(|value| !value.is_null())
        {
            require_exact_object_keys(alias, &["expansion", "depth"], "sealed resolution alias")?;
        }
        Ok(())
    };
    validate_ancestor(&value["root"], "sealed resolution root")?;
    for (field, label) in [
        ("ancestors", "sealed resolution ancestor"),
        ("referenced_items", "sealed resolution referenced item"),
    ] {
        let values = value[field]
            .as_array()
            .ok_or_else(|| format!("sealed resolution {field} must be an array"))?;
        for entry in values {
            validate_ancestor(entry, label)?;
        }
    }
    let edges = value["references_edges"]
        .as_array()
        .ok_or_else(|| "sealed resolution references_edges must be an array".to_string())?;
    for edge in edges {
        require_exact_object_keys(
            edge,
            &[
                "from_ref",
                "from_source_path",
                "to_ref",
                "to_source_path",
                "trust_class",
                "added_by",
            ],
            "sealed resolution edge",
        )?;
    }
    require_exact_object_keys(
        &value["composed"],
        &["composed", "derived", "policy_facts"],
        "sealed resolution composed view",
    )?;
    Ok(())
}

fn deserialize_exact_resolution<'de, D>(
    deserializer: D,
) -> std::result::Result<ryeos_engine::resolution::ResolutionOutput, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    validate_exact_resolution_wire(&value).map_err(serde::de::Error::custom)?;
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

const SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedShadowedCandidate {
    label: String,
    space: ItemSpace,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedItemMetadata {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    executor_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    version: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    description: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    category: Option<String>,
    required_secrets: Vec<String>,
    extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedSourceFormat {
    extension: String,
    parser: String,
    signature_prefix: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    signature_suffix: Option<String>,
    signature_after_shebang: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedResolvedItem {
    canonical_ref: String,
    kind: String,
    source_path: PathBuf,
    source_space: ItemSpace,
    resolved_from: String,
    shadowed: Vec<SealedShadowedCandidate>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    materialized_project_root: Option<PathBuf>,
    raw_content_digest: String,
    content_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    signature_header: Option<SignatureHeader>,
    source_format: SealedSourceFormat,
    metadata: SealedItemMetadata,
}

impl From<&ResolvedItem> for SealedResolvedItem {
    fn from(resolved: &ResolvedItem) -> Self {
        Self {
            canonical_ref: resolved.canonical_ref.to_string(),
            kind: resolved.kind.clone(),
            source_path: resolved.source_path.clone(),
            source_space: resolved.source_space,
            resolved_from: resolved.resolved_from.clone(),
            shadowed: resolved
                .shadowed
                .iter()
                .map(|candidate| SealedShadowedCandidate {
                    label: candidate.label.clone(),
                    space: candidate.space,
                    path: candidate.path.clone(),
                })
                .collect(),
            materialized_project_root: resolved.materialized_project_root.clone(),
            raw_content_digest: resolved.raw_content_digest.clone(),
            content_hash: resolved.content_hash.clone(),
            signature_header: resolved.signature_header.clone(),
            source_format: SealedSourceFormat {
                extension: resolved.source_format.extension.clone(),
                parser: resolved.source_format.parser.clone(),
                signature_prefix: resolved.source_format.signature.prefix.clone(),
                signature_suffix: resolved.source_format.signature.suffix.clone(),
                signature_after_shebang: resolved.source_format.signature.after_shebang,
            },
            metadata: SealedItemMetadata {
                executor_id: resolved.metadata.executor_id.clone(),
                version: resolved.metadata.version.clone(),
                description: resolved.metadata.description.clone(),
                category: resolved.metadata.category.clone(),
                required_secrets: resolved.metadata.required_secrets.clone(),
                extra: resolved.metadata.extra.clone(),
            },
        }
    }
}

impl SealedResolvedItem {
    fn restore(&self) -> Result<ResolvedItem> {
        let canonical_ref = CanonicalRef::parse(&self.canonical_ref)
            .map_err(|error| anyhow!("invalid sealed canonical ref: {error}"))?;
        if canonical_ref.kind != self.kind {
            bail!(
                "sealed resolved kind `{}` does not match canonical ref kind `{}`",
                self.kind,
                canonical_ref.kind
            );
        }
        Ok(ResolvedItem {
            canonical_ref,
            kind: self.kind.clone(),
            source_path: self.source_path.clone(),
            source_space: self.source_space,
            resolved_from: self.resolved_from.clone(),
            shadowed: self
                .shadowed
                .iter()
                .map(|candidate| ShadowedCandidate {
                    label: candidate.label.clone(),
                    space: candidate.space,
                    path: candidate.path.clone(),
                })
                .collect(),
            materialized_project_root: self.materialized_project_root.clone(),
            raw_content_digest: self.raw_content_digest.clone(),
            content_hash: self.content_hash.clone(),
            signature_header: self.signature_header.clone(),
            source_format: ResolvedSourceFormat {
                extension: self.source_format.extension.clone(),
                parser: self.source_format.parser.clone(),
                signature: SignatureEnvelope {
                    prefix: self.source_format.signature_prefix.clone(),
                    suffix: self.source_format.signature_suffix.clone(),
                    after_shebang: self.source_format.signature_after_shebang,
                },
            },
            metadata: ItemMetadata {
                executor_id: self.metadata.executor_id.clone(),
                version: self.metadata.version.clone(),
                description: self.metadata.description.clone(),
                category: self.metadata.category.clone(),
                required_secrets: self.metadata.required_secrets.clone(),
                extra: self.metadata.extra.clone(),
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum SealedPrincipal {
    Local {
        fingerprint: String,
        scopes: Vec<String>,
    },
    Delegated {
        protocol_version: String,
        delegation_id: String,
        caller_fingerprint: String,
        origin_site_id: String,
        audience_site_id: String,
        delegated_scopes: Vec<String>,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        budget_lease_id: Option<String>,
        request_hash: String,
        idempotency_key: String,
        issued_at: String,
        expires_at: String,
        non_redelegable: bool,
        origin_signature: String,
    },
}

impl From<&EffectivePrincipal> for SealedPrincipal {
    fn from(principal: &EffectivePrincipal) -> Self {
        match principal {
            EffectivePrincipal::Local(principal) => Self::Local {
                fingerprint: principal.fingerprint.clone(),
                scopes: principal.scopes.clone(),
            },
            EffectivePrincipal::Delegated(principal) => Self::Delegated {
                protocol_version: principal.protocol_version.clone(),
                delegation_id: principal.delegation_id.clone(),
                caller_fingerprint: principal.caller_fingerprint.clone(),
                origin_site_id: principal.origin_site_id.clone(),
                audience_site_id: principal.audience_site_id.clone(),
                delegated_scopes: principal.delegated_scopes.clone(),
                budget_lease_id: principal.budget_lease_id.clone(),
                request_hash: principal.request_hash.clone(),
                idempotency_key: principal.idempotency_key.clone(),
                issued_at: principal.issued_at.clone(),
                expires_at: principal.expires_at.clone(),
                non_redelegable: principal.non_redelegable,
                origin_signature: principal.origin_signature.clone(),
            },
        }
    }
}

impl SealedPrincipal {
    fn restore(&self) -> EffectivePrincipal {
        match self {
            Self::Local {
                fingerprint,
                scopes,
            } => EffectivePrincipal::Local(Principal {
                fingerprint: fingerprint.clone(),
                scopes: scopes.clone(),
            }),
            Self::Delegated {
                protocol_version,
                delegation_id,
                caller_fingerprint,
                origin_site_id,
                audience_site_id,
                delegated_scopes,
                budget_lease_id,
                request_hash,
                idempotency_key,
                issued_at,
                expires_at,
                non_redelegable,
                origin_signature,
            } => EffectivePrincipal::Delegated(Box::new(DelegatedPrincipal {
                protocol_version: protocol_version.clone(),
                delegation_id: delegation_id.clone(),
                caller_fingerprint: caller_fingerprint.clone(),
                origin_site_id: origin_site_id.clone(),
                audience_site_id: audience_site_id.clone(),
                delegated_scopes: delegated_scopes.clone(),
                budget_lease_id: budget_lease_id.clone(),
                request_hash: request_hash.clone(),
                idempotency_key: idempotency_key.clone(),
                issued_at: issued_at.clone(),
                expires_at: expires_at.clone(),
                non_redelegable: *non_redelegable,
                origin_signature: origin_signature.clone(),
            })),
        }
    }
}

/// Exact, current-format durable authority for a root admitted before its
/// first launch. This is persisted only for the created-root crash window and
/// reconstructs the complete request without consulting mutable item source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedRootExecutionRequest {
    schema_version: u32,
    kind: String,
    item_ref: String,
    executor_ref: String,
    runtime_ref: String,
    launch_mode: String,
    current_site_id: String,
    origin_site_id: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    target_site_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    requested_by: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    usage_subject: Option<UsageSubject>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    usage_subject_asserted_by: Option<String>,
    parameters: Value,
    ref_bindings: BTreeMap<String, String>,
    verified_subject: SealedResolvedItem,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    verified_signer_fingerprint: Option<String>,
    verified_trust_class: TrustClass,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    verified_pinned_version: Option<PinnedVersion>,
    #[serde(deserialize_with = "deserialize_exact_resolution")]
    resolution_output: ryeos_engine::resolution::ResolutionOutput,
    planning_principal: SealedPrincipal,
    project_context: ProjectContext,
    execution_hints: HashMap<String, Value>,
    validate_only: bool,
    resolved_history_policy: ResolvedThreadHistoryPolicy,
    captured_history_policy: ryeos_state::objects::CapturedThreadHistoryPolicy,
}

impl SealedRootExecutionRequest {
    pub fn capture(request: &ResolvedExecutionRequest, runtime_ref: String) -> Result<Self> {
        let admission = request.root_admission.as_ref().ok_or_else(|| {
            anyhow!("cannot seal a root execution request without root admission")
        })?;
        admission.validate()?;
        admission.ensure_matches_request(request)?;
        validate_launch_mode(&request.launch_mode)?;
        if runtime_ref.trim().is_empty() || runtime_ref.trim() != runtime_ref {
            bail!("sealed root execution runtime ref must be non-empty and trimmed");
        }
        CanonicalRef::parse(&runtime_ref)
            .map_err(|error| anyhow!("invalid sealed runtime ref `{runtime_ref}`: {error}"))?;
        let verified = &admission.verified_subject;
        Ok(Self {
            schema_version: SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION,
            kind: request.kind.clone(),
            item_ref: request.item_ref.clone(),
            executor_ref: request.executor_ref.clone(),
            runtime_ref,
            launch_mode: request.launch_mode.clone(),
            current_site_id: request.current_site_id.clone(),
            origin_site_id: request.origin_site_id.clone(),
            target_site_id: request.target_site_id.clone(),
            requested_by: request.requested_by.clone(),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            parameters: request.parameters.clone(),
            ref_bindings: request.ref_bindings.clone(),
            verified_subject: SealedResolvedItem::from(&verified.resolved),
            verified_signer_fingerprint: verified.signer.as_ref().map(|value| value.0.clone()),
            verified_trust_class: verified.trust_class,
            verified_pinned_version: verified.pinned_version.clone(),
            resolution_output: admission.resolution_output.clone(),
            planning_principal: SealedPrincipal::from(&admission.plan_context.requested_by),
            project_context: admission.plan_context.project_context.clone(),
            execution_hints: admission.plan_context.execution_hints.values.clone(),
            validate_only: admission.plan_context.validate_only,
            resolved_history_policy: admission.resolved_history_policy.clone(),
            captured_history_policy: admission.captured_history_policy.clone(),
        })
    }

    pub fn runtime_ref(&self) -> &str {
        &self.runtime_ref
    }

    pub fn executor_ref(&self) -> &str {
        &self.executor_ref
    }

    pub fn item_ref(&self) -> &str {
        &self.item_ref
    }

    /// Structurally complete current-format value for storage-boundary tests.
    /// It is deliberately unavailable in production builds and is never valid
    /// launch authority because its synthetic subject is unsigned.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn storage_test_fixture() -> Self {
        let content_hash = "11".repeat(32);
        let kind_schema_content_hash = "22".repeat(32);
        let canonical_item_ref = "graph:test/storage-fixture".to_string();
        let resolved_history_policy = ResolvedThreadHistoryPolicy {
            retention: ryeos_engine::history_policy::ThreadHistoryRetention::Durable,
            canonical_item_ref: canonical_item_ref.clone(),
            item_content_hash: content_hash.clone(),
            item_signer_fingerprint: None,
            item_trust_class: TrustClass::Unsigned,
            kind_schema_content_hash: kind_schema_content_hash.clone(),
            source: PolicyProvenance::NodeDefault {
                node_policy: NodeHistoryPolicyProvenance::MissingConfig,
            },
        };
        let captured_history_policy = ryeos_state::objects::CapturedThreadHistoryPolicy {
            retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
            canonical_item_ref: canonical_item_ref.clone(),
            item_content_hash: content_hash.clone(),
            item_signer_fingerprint: None,
            item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Unsigned,
            kind_schema_content_hash,
            resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy:
                    ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        };
        Self {
            schema_version: SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION,
            kind: "graph_run".to_string(),
            item_ref: canonical_item_ref.clone(),
            executor_ref: "native:storage-fixture".to_string(),
            runtime_ref: "runtime:storage-fixture".to_string(),
            launch_mode: "detached".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            target_site_id: None,
            requested_by: Some("session:test".to_string()),
            usage_subject: None,
            usage_subject_asserted_by: None,
            parameters: json!({}),
            ref_bindings: BTreeMap::new(),
            verified_subject: SealedResolvedItem {
                canonical_ref: canonical_item_ref.clone(),
                kind: "graph".to_string(),
                source_path: PathBuf::from("/synthetic/storage-fixture.yaml"),
                source_space: ItemSpace::Project,
                resolved_from: "storage_test_fixture".to_string(),
                shadowed: Vec::new(),
                materialized_project_root: None,
                raw_content_digest: content_hash.clone(),
                content_hash: content_hash.clone(),
                signature_header: None,
                source_format: SealedSourceFormat {
                    extension: "yaml".to_string(),
                    parser: "yaml-header-document".to_string(),
                    signature_prefix: "# ".to_string(),
                    signature_suffix: None,
                    signature_after_shebang: false,
                },
                metadata: SealedItemMetadata {
                    executor_id: None,
                    version: None,
                    description: None,
                    category: None,
                    required_secrets: Vec::new(),
                    extra: HashMap::new(),
                },
            },
            verified_signer_fingerprint: None,
            verified_trust_class: TrustClass::Unsigned,
            verified_pinned_version: None,
            resolution_output: ryeos_engine::resolution::ResolutionOutput {
                root: ryeos_engine::resolution::ResolvedAncestor {
                    requested_id: canonical_item_ref.clone(),
                    resolved_ref: canonical_item_ref,
                    source_path: PathBuf::from("/synthetic/storage-fixture.yaml"),
                    trust_class: ResolutionTrustClass::Unsigned,
                    alias_resolution: None,
                    added_by: ryeos_engine::resolution::ResolutionStepName::PipelineInit,
                    raw_content: "{}".to_string(),
                    raw_content_digest: content_hash,
                },
                ancestors: Vec::new(),
                references_edges: Vec::new(),
                referenced_items: Vec::new(),
                step_outputs: HashMap::new(),
                effective_trust_class: ResolutionTrustClass::Unsigned,
                composed: ryeos_engine::resolution::KindComposedView::identity(json!({})),
            },
            planning_principal: SealedPrincipal::Local {
                fingerprint: "session:test".to_string(),
                scopes: Vec::new(),
            },
            project_context: ProjectContext::None,
            execution_hints: HashMap::new(),
            validate_only: false,
            resolved_history_policy,
            captured_history_policy,
        }
    }

    pub fn restore(&self, engine: &Engine) -> Result<ResolvedExecutionRequest> {
        if self.schema_version != SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION {
            bail!(
                "sealed root execution request schema mismatch: persisted={}, expected={}",
                self.schema_version,
                SEALED_ROOT_EXECUTION_REQUEST_SCHEMA_VERSION
            );
        }
        if self.validate_only {
            bail!("persisted root execution request cannot be validate-only");
        }
        validate_launch_mode(&self.launch_mode)?;
        if self.runtime_ref.trim().is_empty() || self.runtime_ref.trim() != self.runtime_ref {
            bail!("sealed root execution runtime ref must be non-empty and trimmed");
        }
        CanonicalRef::parse(&self.runtime_ref).map_err(|error| {
            anyhow!("invalid sealed runtime ref `{}`: {error}", self.runtime_ref)
        })?;
        let resolved_item = self.verified_subject.restore()?;
        let verified_subject = VerifiedItem {
            resolved: resolved_item.clone(),
            signer: self
                .verified_signer_fingerprint
                .clone()
                .map(SignerFingerprint),
            trust_class: self.verified_trust_class,
            pinned_version: self.verified_pinned_version.clone(),
        };
        let plan_context = PlanContext {
            requested_by: self.planning_principal.restore(),
            project_context: self.project_context.clone(),
            current_site_id: self.current_site_id.clone(),
            origin_site_id: self.origin_site_id.clone(),
            execution_hints: ExecutionHints {
                values: self.execution_hints.clone(),
            },
            validate_only: false,
        };
        let admission = RootExecutionAdmission {
            verified_subject,
            resolution_output: self.resolution_output.clone(),
            plan_context: plan_context.clone(),
            thread_profile: self.kind.clone(),
            usage_subject: self.usage_subject.clone(),
            usage_subject_asserted_by: self.usage_subject_asserted_by.clone(),
            ref_bindings: self.ref_bindings.clone(),
            resolved_history_policy: self.resolved_history_policy.clone(),
            captured_history_policy: self.captured_history_policy.clone(),
        };
        admission.validate_for_persistence(engine)?;
        let request = ResolvedExecutionRequest {
            kind: self.kind.clone(),
            item_ref: self.item_ref.clone(),
            executor_ref: self.executor_ref.clone(),
            launch_mode: self.launch_mode.clone(),
            current_site_id: self.current_site_id.clone(),
            origin_site_id: self.origin_site_id.clone(),
            target_site_id: self.target_site_id.clone(),
            requested_by: self.requested_by.clone(),
            usage_subject: self.usage_subject.clone(),
            usage_subject_asserted_by: self.usage_subject_asserted_by.clone(),
            parameters: self.parameters.clone(),
            ref_bindings: self.ref_bindings.clone(),
            root_raw_content_digest: resolved_item.raw_content_digest.clone(),
            resolved_item,
            plan_context,
            root_admission: Some(admission.clone()),
        };
        admission.ensure_matches_request(&request)?;
        Ok(request)
    }
}

/// Exact verified subject and destructive-history authority admitted for one
/// new execution root. The verified item is intentionally carried as data:
/// background dispatch must not turn an already-admitted canonical ref back
/// into a fresh mutable-filesystem policy lookup.
#[derive(Debug, Clone)]
pub struct RootExecutionAdmission {
    verified_subject: VerifiedItem,
    resolution_output: ryeos_engine::resolution::ResolutionOutput,
    plan_context: PlanContext,
    thread_profile: String,
    usage_subject: Option<UsageSubject>,
    usage_subject_asserted_by: Option<String>,
    ref_bindings: BTreeMap<String, String>,
    resolved_history_policy: ResolvedThreadHistoryPolicy,
    captured_history_policy: ryeos_state::objects::CapturedThreadHistoryPolicy,
}

impl RootExecutionAdmission {
    pub fn verified_subject(&self) -> &VerifiedItem {
        &self.verified_subject
    }

    /// Exact verified composition snapshot captured before root persistence.
    /// Fresh dispatch and launch consume this value instead of re-reading the
    /// mutable item source.
    pub fn resolution_output(&self) -> &ryeos_engine::resolution::ResolutionOutput {
        &self.resolution_output
    }

    pub fn thread_profile(&self) -> &str {
        &self.thread_profile
    }

    pub fn resolved_history_policy(&self) -> &ResolvedThreadHistoryPolicy {
        &self.resolved_history_policy
    }

    pub fn captured_history_policy(&self) -> &ryeos_state::objects::CapturedThreadHistoryPolicy {
        &self.captured_history_policy
    }

    pub fn ref_bindings(&self) -> &BTreeMap<String, String> {
        &self.ref_bindings
    }

    pub fn project_root(&self) -> Option<&Path> {
        match &self.plan_context.project_context {
            ProjectContext::LocalPath { path } => Some(path.as_path()),
            ProjectContext::None
            | ProjectContext::SnapshotHash { .. }
            | ProjectContext::ProjectRef { .. } => None,
        }
    }

    /// Build the exact fresh-root request represented by this admission. Child
    /// launchers use this rather than reconstructing request authority from
    /// row fields or mutable item source.
    pub fn execution_request(
        &self,
        executor_ref: String,
        launch_mode: String,
        parameters: Value,
    ) -> Result<ResolvedExecutionRequest> {
        validate_launch_mode(&launch_mode)?;
        let resolved = &self.verified_subject.resolved;
        let request = ResolvedExecutionRequest {
            kind: self.thread_profile.clone(),
            item_ref: resolved.canonical_ref.to_string(),
            executor_ref,
            launch_mode,
            current_site_id: self.plan_context.current_site_id.clone(),
            origin_site_id: self.plan_context.origin_site_id.clone(),
            target_site_id: None,
            requested_by: Some(plan_principal_identifier(&self.plan_context).to_string()),
            usage_subject: self.usage_subject.clone(),
            usage_subject_asserted_by: self.usage_subject_asserted_by.clone(),
            parameters,
            ref_bindings: self.ref_bindings.clone(),
            root_raw_content_digest: resolved.raw_content_digest.clone(),
            resolved_item: resolved.clone(),
            plan_context: self.plan_context.clone(),
            root_admission: Some(self.clone()),
        };
        self.ensure_matches_request(&request)?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<()> {
        validate_principal_identifier(
            "admitted root planning principal",
            plan_principal_identifier(&self.plan_context),
        )?;
        self.captured_history_policy.validate()?;
        if capture_thread_history_policy(&self.resolved_history_policy)?
            != self.captured_history_policy
        {
            bail!("admitted root resolved and captured history policies differ");
        }
        if self.thread_profile.trim().is_empty()
            || self.thread_profile.trim() != self.thread_profile
        {
            bail!("admitted root thread profile must be non-empty and trimmed");
        }
        match (&self.usage_subject, &self.usage_subject_asserted_by) {
            (Some(subject), Some(asserted_by)) => {
                subject.validate()?;
                if asserted_by.trim().is_empty() || asserted_by.trim() != asserted_by {
                    bail!("admitted root usage authority must be non-empty and trimmed");
                }
                if asserted_by != plan_principal_identifier(&self.plan_context) {
                    bail!(
                        "admitted root usage authority does not match the sealed planning principal"
                    );
                }
            }
            (None, None) => {}
            _ => bail!(
                "admitted root usage subject and asserting authority must be supplied together"
            ),
        }
        let resolved = &self.verified_subject.resolved;
        let canonical = resolved.canonical_ref.to_string();
        if resolved.kind != resolved.canonical_ref.kind {
            bail!(
                "admitted root subject kind mismatch: resolved `{}` but canonical ref is `{}`",
                resolved.kind,
                resolved.canonical_ref.kind
            );
        }
        if self.captured_history_policy.canonical_item_ref != canonical {
            bail!(
                "admitted root history subject mismatch: policy names `{}` but verified subject is `{canonical}`",
                self.captured_history_policy.canonical_item_ref
            );
        }
        if self.captured_history_policy.item_content_hash != resolved.content_hash {
            bail!(
                "admitted root content mismatch for `{canonical}`: policy hash `{}` differs from verified hash `{}`",
                self.captured_history_policy.item_content_hash,
                resolved.content_hash
            );
        }
        if self.resolution_output.root.resolved_ref != canonical {
            bail!(
                "admitted root resolution subject mismatch: composed `{}` but verified `{canonical}`",
                self.resolution_output.root.resolved_ref
            );
        }
        let resolution_digest =
            lillux::signature::content_hash(&self.resolution_output.root.raw_content);
        if self.resolution_output.root.raw_content_digest != resolution_digest {
            bail!("admitted root resolution digest is internally inconsistent for `{canonical}`");
        }
        let expected_resolution_digest = resolved
            .signature_header
            .as_ref()
            .map(|header| header.content_hash.as_str())
            .unwrap_or(resolved.content_hash.as_str());
        if resolution_digest != expected_resolution_digest {
            bail!("admitted root resolution bytes do not match the verified subject `{canonical}`");
        }
        let signer = verified_item_signer(&self.verified_subject)?;
        if self.captured_history_policy.item_signer_fingerprint != signer {
            bail!("admitted root signer mismatch for `{canonical}`");
        }
        let trust = capture_item_trust_class(self.verified_subject.trust_class);
        if self.captured_history_policy.item_trust_class != trust {
            bail!("admitted root trust-class mismatch for `{canonical}`");
        }
        Ok(())
    }

    /// Bind a dispatch leaf back to the admitted canonical/hash/signer/trust
    /// identity and verified kind schema. Mutable path and shadow diagnostics
    /// are deliberately not authority; the signed content identity is.
    pub fn ensure_matches_subject(
        &self,
        engine: &Engine,
        verified: &VerifiedItem,
        thread_profile: &str,
    ) -> Result<()> {
        self.validate()?;
        let expected = &self.verified_subject;
        let expected_resolved = &expected.resolved;
        let actual = &verified.resolved;
        let matches = expected_resolved.canonical_ref == actual.canonical_ref
            && expected_resolved.kind == actual.kind
            && expected_resolved.content_hash == actual.content_hash
            && expected.trust_class == verified.trust_class
            && verified_item_signer(expected)? == verified_item_signer(verified)?;
        if !matches {
            bail!(
                "dispatch subject `{}` does not match the exact subject admitted for `{}`",
                actual.canonical_ref,
                expected_resolved.canonical_ref
            );
        }
        if self.thread_profile != thread_profile {
            bail!(
                "dispatch thread profile `{thread_profile}` does not match admitted profile `{}` for `{}`",
                self.thread_profile,
                actual.canonical_ref
            );
        }
        let schema_hash = engine
            .kinds
            .schema_content_hash(&actual.kind)
            .ok_or_else(|| {
                anyhow!(
                    "admitted subject kind `{}` has no verified schema hash",
                    actual.kind
                )
            })?;
        if schema_hash != self.captured_history_policy.kind_schema_content_hash {
            bail!(
                "kind schema for admitted subject `{}` changed: admitted `{}`, current `{schema_hash}`",
                actual.canonical_ref,
                self.captured_history_policy.kind_schema_content_hash
            );
        }
        let declared_profile = engine
            .kinds
            .get(&actual.kind)
            .and_then(|schema| schema.execution())
            .and_then(|execution| execution.thread_profile.as_ref())
            .map(|profile| profile.name.as_str());
        if declared_profile != Some(self.thread_profile.as_str()) {
            bail!(
                "kind schema thread profile for admitted subject `{}` no longer matches `{}`",
                actual.canonical_ref,
                self.thread_profile
            );
        }
        Ok(())
    }

    pub fn ensure_matches_request(&self, request: &ResolvedExecutionRequest) -> Result<()> {
        self.validate()?;
        if !same_plan_context(&self.plan_context, &request.plan_context) {
            bail!(
                "resolved execution request planning authority does not match the sealed root admission"
            );
        }
        if request.usage_subject != self.usage_subject
            || request.usage_subject_asserted_by != self.usage_subject_asserted_by
        {
            bail!(
                "resolved execution request usage attribution does not match the sealed root admission"
            );
        }
        if request.ref_bindings != self.ref_bindings {
            bail!("resolved execution request secondary identities do not match the sealed root admission");
        }
        if request.current_site_id != self.plan_context.current_site_id
            || request.origin_site_id != self.plan_context.origin_site_id
            || request.requested_by.as_deref()
                != Some(plan_principal_identifier(&self.plan_context))
        {
            bail!("resolved execution request row authority does not match its PlanContext");
        }
        let expected = &self.verified_subject.resolved;
        if request.item_ref != expected.canonical_ref.to_string()
            || request.resolved_item.canonical_ref != expected.canonical_ref
            || request.resolved_item.kind != expected.kind
            || request.resolved_item.content_hash != expected.content_hash
            || request.kind != self.thread_profile
        {
            bail!(
                "resolved execution request does not match admitted root subject `{}`",
                expected.canonical_ref
            );
        }
        Ok(())
    }

    /// Validate the sealed admission at the final persistence boundary against
    /// the already-loaded verified kind registry. The mutable item source is
    /// deliberately not re-resolved or re-read after admission.
    fn validate_for_persistence(&self, engine: &Engine) -> Result<()> {
        self.ensure_matches_subject(engine, &self.verified_subject, &self.thread_profile)
    }
}

fn same_plan_context(left: &PlanContext, right: &PlanContext) -> bool {
    left.requested_by == right.requested_by
        && left.project_context == right.project_context
        && left.current_site_id == right.current_site_id
        && left.origin_site_id == right.origin_site_id
        && left.execution_hints == right.execution_hints
        && left.validate_only == right.validate_only
}

fn plan_principal_identifier(context: &PlanContext) -> &str {
    match &context.requested_by {
        EffectivePrincipal::Local(principal) => &principal.fingerprint,
        EffectivePrincipal::Delegated(delegated) => &delegated.caller_fingerprint,
    }
}

/// Validate a canonical in-memory principal identifier without assuming an
/// authentication scheme. Execution and non-execution roots may be owned by
/// verified node fingerprints, daemon-minted session principals, or another
/// canonical scheme; none becomes a filesystem key at this boundary.
fn validate_principal_identifier(label: &str, principal: &str) -> Result<()> {
    if principal.is_empty() || principal.trim() != principal {
        bail!("{label} must be non-empty and trimmed");
    }
    if principal.chars().any(char::is_control) {
        bail!("{label} must not contain control characters");
    }
    let (prefix, subject) = principal
        .split_once(':')
        .ok_or_else(|| anyhow!("{label} must use a structured `<prefix>:<subject>` identifier"))?;
    let mut prefix_bytes = prefix.bytes();
    if !prefix_bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase())
        || !prefix_bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'.')
        })
    {
        bail!("{label} has a non-canonical structured prefix");
    }
    if subject.is_empty() {
        bail!("{label} must include a non-empty subject after its prefix");
    }
    Ok(())
}

/// Sealed authority for daemon-owned roots which do not execute a kind
/// profile (for example an operator seat). It binds the final row to the exact
/// verified subject and captured policy; callers cannot pair a policy resolved
/// from one item with another `item_ref`.
#[derive(Debug, Clone)]
pub struct NonExecutionRootAdmission {
    verified_subject: VerifiedItem,
    plan_context: PlanContext,
    thread_profile: String,
    resolved_history_policy: ResolvedThreadHistoryPolicy,
    captured_history_policy: ryeos_state::objects::CapturedThreadHistoryPolicy,
}

impl NonExecutionRootAdmission {
    pub fn verified_subject(&self) -> &VerifiedItem {
        &self.verified_subject
    }

    pub fn captured_history_policy(&self) -> &ryeos_state::objects::CapturedThreadHistoryPolicy {
        &self.captured_history_policy
    }

    pub fn thread_profile(&self) -> &str {
        &self.thread_profile
    }

    pub fn project_root(&self) -> Option<&Path> {
        match &self.plan_context.project_context {
            ProjectContext::LocalPath { path } => Some(path.as_path()),
            ProjectContext::None
            | ProjectContext::SnapshotHash { .. }
            | ProjectContext::ProjectRef { .. } => None,
        }
    }

    fn validate(&self) -> Result<()> {
        validate_principal_identifier(
            "non-execution root planning principal",
            plan_principal_identifier(&self.plan_context),
        )?;
        if self.thread_profile.trim().is_empty()
            || self.thread_profile.trim() != self.thread_profile
        {
            bail!("non-execution root protocol profile must be non-empty and trimmed");
        }
        self.captured_history_policy.validate()?;
        if capture_thread_history_policy(&self.resolved_history_policy)?
            != self.captured_history_policy
        {
            bail!("non-execution root resolved and captured history policies differ");
        }
        let resolved = &self.verified_subject.resolved;
        let canonical = resolved.canonical_ref.to_string();
        if resolved.kind != resolved.canonical_ref.kind
            || self.captured_history_policy.canonical_item_ref != canonical
            || self.captured_history_policy.item_content_hash != resolved.content_hash
            || self.captured_history_policy.item_signer_fingerprint
                != verified_item_signer(&self.verified_subject)?
            || self.captured_history_policy.item_trust_class
                != capture_item_trust_class(self.verified_subject.trust_class)
        {
            bail!("non-execution root admission does not bind its verified subject");
        }
        Ok(())
    }

    /// Validate the sealed admission at the final persistence boundary against
    /// the already-loaded verified kind registry. The mutable item source is
    /// deliberately not re-resolved or re-read after admission.
    fn validate_for_persistence(&self, engine: &Engine) -> Result<()> {
        self.validate()?;
        let admitted = &self.verified_subject.resolved;
        let current_schema_hash = engine
            .kinds
            .schema_content_hash(&admitted.kind)
            .ok_or_else(|| {
                anyhow!(
                    "non-execution root kind `{}` has no verified schema hash",
                    admitted.kind
                )
            })?;
        if current_schema_hash != self.captured_history_policy.kind_schema_content_hash {
            bail!(
                "kind schema for non-execution root `{}` changed after admission",
                admitted.canonical_ref
            );
        }
        Ok(())
    }
}

fn verified_item_signer(verified: &VerifiedItem) -> Result<Option<String>> {
    let canonical = &verified.resolved.canonical_ref;
    match verified.trust_class {
        TrustClass::Unsigned => {
            if verified.signer.is_some() || verified.resolved.signature_header.is_some() {
                bail!(
                    "unsigned admitted subject `{canonical}` unexpectedly carries signed provenance"
                );
            }
            Ok(None)
        }
        TrustClass::Trusted | TrustClass::Untrusted => {
            let signer = verified.signer.as_ref().ok_or_else(|| {
                anyhow!("signed admitted subject `{canonical}` has no verified signer")
            })?;
            let header = verified.resolved.signature_header.as_ref().ok_or_else(|| {
                anyhow!("signed admitted subject `{canonical}` has no signature header")
            })?;
            if signer.0 != header.signer_fingerprint {
                bail!(
                    "signed admitted subject `{canonical}` has conflicting verified and header signers"
                );
            }
            if header.content_hash != verified.resolved.content_hash {
                bail!(
                    "signed admitted subject `{canonical}` has conflicting verified and header content hashes"
                );
            }
            Ok(Some(signer.0.clone()))
        }
    }
}

fn capture_item_trust_class(trust: TrustClass) -> ryeos_state::objects::CapturedItemTrustClass {
    match trust {
        TrustClass::Trusted => ryeos_state::objects::CapturedItemTrustClass::Trusted,
        TrustClass::Untrusted => ryeos_state::objects::CapturedItemTrustClass::Untrusted,
        TrustClass::Unsigned => ryeos_state::objects::CapturedItemTrustClass::Unsigned,
    }
}

/// Convert the engine launch contract into authoritative state wire data at
/// the app boundary. State intentionally has no dependency on engine types.
pub fn capture_thread_history_policy(
    policy: &ResolvedThreadHistoryPolicy,
) -> Result<ryeos_state::objects::CapturedThreadHistoryPolicy> {
    let captured = ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: match &policy.retention {
            ryeos_engine::history_policy::ThreadHistoryRetention::Durable => {
                ryeos_state::objects::ThreadHistoryRetention::Durable
            }
            ryeos_engine::history_policy::ThreadHistoryRetention::TerminalFor { seconds } => {
                ryeos_state::objects::ThreadHistoryRetention::TerminalFor { seconds: *seconds }
            }
        },
        canonical_item_ref: policy.canonical_item_ref.clone(),
        item_content_hash: policy.item_content_hash.clone(),
        item_signer_fingerprint: policy.item_signer_fingerprint.clone(),
        item_trust_class: match policy.item_trust_class {
            TrustClass::Trusted => ryeos_state::objects::CapturedItemTrustClass::Trusted,
            TrustClass::Untrusted => ryeos_state::objects::CapturedItemTrustClass::Untrusted,
            TrustClass::Unsigned => ryeos_state::objects::CapturedItemTrustClass::Unsigned,
        },
        kind_schema_content_hash: policy.kind_schema_content_hash.clone(),
        resolved_from: capture_policy_provenance(&policy.source)?,
    };
    captured.validate()?;
    Ok(captured)
}

fn capture_item_space(space: ItemSpace) -> ryeos_state::objects::CapturedItemSpace {
    match space {
        ItemSpace::Project => ryeos_state::objects::CapturedItemSpace::Project,
        ItemSpace::Bundle => ryeos_state::objects::CapturedItemSpace::Bundle,
    }
}

fn capture_node_policy_provenance(
    provenance: &NodeHistoryPolicyProvenance,
) -> Result<ryeos_state::objects::CapturedNodeHistoryPolicyProvenance> {
    match provenance {
        NodeHistoryPolicyProvenance::MissingConfig => {
            Ok(ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig)
        }
        NodeHistoryPolicyProvenance::SignedConfig {
            path,
            space,
            content_hash,
            signer_fingerprint,
        } => {
            if path != Path::new(ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG) {
                bail!(
                    "node history provenance path must be exactly `{}`",
                    ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG
                );
            }
            Ok(
                ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::SignedConfig {
                    path: PathBuf::from(ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG),
                    space: capture_item_space(*space),
                    content_hash: content_hash.clone(),
                    signer_fingerprint: signer_fingerprint.clone(),
                },
            )
        }
    }
}

fn capture_effective_trust_class(
    trust: ResolutionTrustClass,
) -> ryeos_state::objects::CapturedEffectiveTrustClass {
    match trust {
        ResolutionTrustClass::TrustedBundle => {
            ryeos_state::objects::CapturedEffectiveTrustClass::TrustedBundle
        }
        ResolutionTrustClass::TrustedProject => {
            ryeos_state::objects::CapturedEffectiveTrustClass::TrustedProject
        }
        ResolutionTrustClass::UntrustedProject => {
            ryeos_state::objects::CapturedEffectiveTrustClass::UntrustedProject
        }
        ResolutionTrustClass::Unsigned => {
            ryeos_state::objects::CapturedEffectiveTrustClass::Unsigned
        }
    }
}

fn capture_policy_provenance(
    provenance: &PolicyProvenance,
) -> Result<ryeos_state::objects::CapturedPolicyProvenance> {
    match provenance {
        PolicyProvenance::NodeDefault { node_policy } => Ok(
            ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                node_policy: capture_node_policy_provenance(node_policy)?,
            },
        ),
        PolicyProvenance::ItemAuthored {
            composed_path,
            requested_seconds,
            effective_trust_class,
            minimum_clamp,
            node_policy,
        } => Ok(
            ryeos_state::objects::CapturedPolicyProvenance::ItemAuthored {
                composed_path: composed_path.clone(),
                requested_seconds: *requested_seconds,
                effective_trust_class: capture_effective_trust_class(*effective_trust_class),
                minimum_clamp: minimum_clamp.as_ref().map(|clamp| {
                    ryeos_state::objects::CapturedThreadHistoryMinimumClamp {
                        requested_seconds: clamp.requested_seconds,
                        minimum_seconds: clamp.minimum_seconds,
                    }
                }),
                node_policy: capture_node_policy_provenance(node_policy)?,
            },
        ),
    }
}

impl ThreadLifecycleService {
    pub fn new(
        state_store: Arc<StateStore>,
        engine: Arc<Engine>,
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
            engine,
            kind_profiles,
            _events,
            event_hub,
            current_site_id: format!("site:{hostname}"),
            scheduler_db: std::sync::RwLock::new(None),
            scheduler_runtime_gate: std::sync::RwLock::new(None),
            app_root: std::sync::RwLock::new(None),
            live_input: std::sync::RwLock::new(None),
        })
    }

    /// Wire the shared operator live-input queue. Called once after
    /// construction with the same `Arc` held by `AppState`, so finalization can
    /// close a thread's entry. Mirrors `set_scheduler_db`.
    pub fn set_live_input_queue(&self, queue: Arc<crate::live_input_queue::LiveInputQueue>) {
        *self.live_input.write().unwrap() = Some(queue);
    }

    /// Close a thread's live-input entry at terminal, if a queue is wired.
    /// Clears any queued inputs and rejects future enqueues. No-op when unset.
    fn close_live_input(&self, thread_id: &str) {
        if let Some(queue) = self.live_input.read().unwrap().as_ref() {
            queue.close(thread_id);
        }
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
        runtime_gate: Arc<tokio::sync::RwLock<()>>,
        app_root: std::path::PathBuf,
    ) {
        *self.scheduler_db.write().unwrap() = Some(db);
        *self.scheduler_runtime_gate.write().unwrap() = Some(runtime_gate);
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
        let admission = request.root_admission.as_ref().ok_or_else(|| {
            anyhow!(
                "new root `{thread_id}` has no admitted execution contract; continuation/existing-row history cannot be used for root creation"
            )
        })?;
        admission.validate_for_persistence(&self.engine)?;
        admission.ensure_matches_request(request)?;
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
            project_root: local_project_root(&request.plan_context),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            captured_history_policy: Some(admission.captured_history_policy.clone()),
        };

        let persisted = self
            .state_store
            .create_admitted_root_thread(&thread_record)?;
        self.publish_records(&persisted);

        self.get_thread(thread_id)?
            .ok_or_else(|| anyhow!("created thread missing from database: {thread_id}"))
    }

    /// Managed-launch root creation with an authoritative initial event set.
    /// Publishes only after the root snapshot and all events commit together.
    pub fn create_root_thread_with_events(
        &self,
        thread_id: &str,
        request: &ResolvedExecutionRequest,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<ThreadDetail> {
        self.create_root_thread_with_events_and_launch_metadata(
            thread_id,
            request,
            initial_events,
            None,
        )
    }

    /// Managed-launch root creation with its durable resume identity prepared
    /// before the authoritative root commit is published.
    pub fn create_root_thread_with_events_and_launch_metadata(
        &self,
        thread_id: &str,
        request: &ResolvedExecutionRequest,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
    ) -> Result<ThreadDetail> {
        validate_kind(&request.kind, self.kind_profiles())?;
        validate_thread_id_format(thread_id)?;
        let admission = request.root_admission.as_ref().ok_or_else(|| {
            anyhow!("managed root `{thread_id}` has no admitted execution contract")
        })?;
        admission.validate_for_persistence(&self.engine)?;
        admission.ensure_matches_request(request)?;
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
            project_root: local_project_root(&request.plan_context),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            captured_history_policy: Some(admission.captured_history_policy.clone()),
        };
        let persisted = self
            .state_store
            .create_root_thread_with_events_and_launch_metadata(
                &thread_record,
                initial_events,
                launch_metadata,
            )?;
        let detail = self.get_thread(thread_id)?
            .ok_or_else(|| anyhow!("created root thread missing from database: {thread_id}"))?;
        self.publish_records(&persisted);
        Ok(detail)
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
        initial_events: Vec<NewEventRecord>,
    ) -> Result<ThreadDetail> {
        self.create_continuation_with_id_and_launch_metadata(
            successor_thread_id,
            source_thread_id,
            request,
            reason,
            initial_events,
            None,
        )
    }

    pub fn create_continuation_with_id_and_launch_metadata(
        &self,
        successor_thread_id: &str,
        source_thread_id: &str,
        request: &ResolvedExecutionRequest,
        reason: Option<&str>,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
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
            project_root: local_project_root(&request.plan_context),
            usage_subject: request.usage_subject.clone(),
            usage_subject_asserted_by: request.usage_subject_asserted_by.clone(),
            captured_history_policy: None,
        };

        let persisted = self.state_store.create_continuation_admitted(
            &successor_record,
            &source.thread_id,
            &source.chain_root_id,
            reason,
            initial_events,
            launch_metadata,
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
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        initial_events: Vec<NewEventRecord>,
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
        let outcome = self.state_store.create_or_get_continuation_admitted(
            successor,
            &source.thread_id,
            &source.chain_root_id,
            reason,
            request_fingerprint,
            Some(launch_metadata),
            initial_events,
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
        if params.thread_id == params.chain_root_id {
            bail!(
                "root `{}` must be created through an admitted execution or explicit non-execution root boundary",
                params.thread_id
            );
        }
        if params.captured_history_policy.is_some() {
            bail!("non-root threads cannot carry a captured history policy");
        }
        self.persist_thread(params)
    }

    fn persist_thread(&self, params: &ThreadCreateParams) -> Result<ThreadDetail> {
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
            project_root: params.project_root.clone(),
            usage_subject: params.usage_subject.clone(),
            usage_subject_asserted_by: params.usage_subject_asserted_by.clone(),
            captured_history_policy: if params.thread_id == params.chain_root_id {
                params.captured_history_policy.clone()
            } else {
                None
            },
        };

        let persisted = if params.thread_id == params.chain_root_id {
            self.state_store
                .create_admitted_root_thread(&thread_record)?
        } else {
            self.state_store
                .create_child_thread_admitted(&thread_record)?
        };
        self.publish_records(&persisted);

        self.get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("created thread missing from database: {}", params.thread_id))
    }

    fn validate_root_create_shape(&self, params: &ThreadCreateParams) -> Result<()> {
        if params.thread_id != params.chain_root_id {
            bail!(
                "root creation requires thread_id == chain_root_id ({} != {})",
                params.thread_id,
                params.chain_root_id
            );
        }
        if params.upstream_thread_id.is_some() {
            bail!("root creation cannot carry an upstream_thread_id");
        }
        Ok(())
    }

    /// Persist a new executable root only when its final row fields still bind
    /// to the exact verified admission produced before the thread ID existed.
    /// Callers cannot substitute a captured policy from another subject or
    /// silently change the schema-derived thread profile.
    pub fn create_admitted_root_thread(
        &self,
        params: &ThreadCreateParams,
        admission: &RootExecutionAdmission,
    ) -> Result<ThreadDetail> {
        self.validate_root_create_shape(params)?;
        admission.validate_for_persistence(&self.engine)?;
        let admitted_ref = admission
            .verified_subject
            .resolved
            .canonical_ref
            .to_string();
        if params.item_ref != admitted_ref {
            bail!(
                "root item_ref `{}` does not match admitted subject `{admitted_ref}`",
                params.item_ref
            );
        }
        if params.kind != admission.thread_profile {
            bail!(
                "root thread profile `{}` does not match admitted profile `{}` for `{admitted_ref}`",
                params.kind,
                admission.thread_profile
            );
        }
        if params.current_site_id != admission.plan_context.current_site_id
            || params.origin_site_id != admission.plan_context.origin_site_id
        {
            bail!("root site identity does not match the sealed planning authority");
        }
        if params.requested_by.as_deref()
            != Some(plan_principal_identifier(&admission.plan_context))
        {
            bail!("root principal does not match the sealed planning authority");
        }
        let admitted_project_root = match &admission.plan_context.project_context {
            ProjectContext::LocalPath { path } => Some(path.as_path()),
            ProjectContext::None
            | ProjectContext::SnapshotHash { .. }
            | ProjectContext::ProjectRef { .. } => None,
        };
        if params.project_root.as_deref() != admitted_project_root {
            bail!("root project path does not match the sealed planning authority");
        }
        if params.usage_subject != admission.usage_subject
            || params.usage_subject_asserted_by != admission.usage_subject_asserted_by
        {
            bail!("root usage attribution does not match the sealed admission authority");
        }
        if params.captured_history_policy.is_some() {
            bail!("root captured history policy is supplied only by sealed admission authority");
        }
        let mut admitted = params.clone();
        admitted.captured_history_policy = Some(admission.captured_history_policy.clone());
        self.persist_thread(&admitted)
    }

    /// Persist a daemon-owned non-execution root (for example an operator seat)
    /// after its generic verified history policy has been captured. Executable
    /// roots must use [`Self::create_admitted_root_thread`].
    pub fn create_non_execution_root_thread(
        &self,
        params: &ThreadCreateParams,
        admission: &NonExecutionRootAdmission,
    ) -> Result<ThreadDetail> {
        self.validate_root_create_shape(params)?;
        admission.validate_for_persistence(&self.engine)?;
        let admitted_ref = admission
            .verified_subject
            .resolved
            .canonical_ref
            .to_string();
        if params.item_ref != admitted_ref {
            bail!(
                "non-execution root item_ref `{}` does not match admitted subject `{admitted_ref}`",
                params.item_ref
            );
        }
        if params.kind != admission.thread_profile {
            bail!(
                "non-execution root profile `{}` does not match admitted protocol profile `{}`",
                params.kind,
                admission.thread_profile
            );
        }
        if params.current_site_id != admission.plan_context.current_site_id
            || params.origin_site_id != admission.plan_context.origin_site_id
            || params.requested_by.as_deref()
                != Some(plan_principal_identifier(&admission.plan_context))
        {
            bail!("non-execution root row authority does not match its sealed PlanContext");
        }
        if params.project_root.as_deref() != admission.project_root() {
            bail!("non-execution root project path does not match its sealed PlanContext");
        }
        if params.captured_history_policy.is_some() {
            bail!("root captured history policy is supplied only by sealed admission authority");
        }
        let mut admitted = params.clone();
        admitted.captured_history_policy = Some(admission.captured_history_policy.clone());
        self.persist_thread(&admitted)
    }

    /// Lifecycle fixture for tests below the engine-admission boundary. This
    /// API is not compiled into production and requires the fixture itself to
    /// carry a valid captured policy for roots.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_thread_for_test(&self, params: &ThreadCreateParams) -> Result<ThreadDetail> {
        if params.thread_id == params.chain_root_id && params.captured_history_policy.is_none() {
            bail!("test root fixture requires a captured history policy");
        }
        self.persist_thread(params)
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
        let process_identity = params.process_identity.as_ref().ok_or_else(|| {
            anyhow!(
                "refusing to attach process {}/{} without a durable process identity",
                params.pid,
                params.pgid
            )
        })?;
        self.state_store.attach_thread_process(
            &params.thread_id,
            params.pid,
            params.pgid,
            process_identity,
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
        managed_envelope: Option<Value>,
    ) -> Result<ThreadDetail> {
        self.finalize_from_completion_inner(thread_id, completion, managed_envelope, false)
    }

    /// Finalization reported by the runtime callback boundary. The StateStore
    /// atomically fences daemon shutdown and lets durable Cancel/Kill intent
    /// dominate the runtime's self-reported terminal status.
    pub fn finalize_from_runtime_completion(
        &self,
        thread_id: &str,
        completion: &ExecutionCompletion,
        managed_envelope: Option<Value>,
    ) -> Result<ThreadDetail> {
        self.finalize_from_completion_inner(thread_id, completion, managed_envelope, true)
    }

    fn finalize_from_completion_inner(
        &self,
        thread_id: &str,
        completion: &ExecutionCompletion,
        managed_envelope: Option<Value>,
        runtime_callback: bool,
    ) -> Result<ThreadDetail> {
        let reported_status = completion.status.as_str();
        let reported_outcome_code = completion.outcome_code.clone().or_else(|| {
            Some(if completion.status == ThreadTerminalStatus::Completed {
                "success".to_string()
            } else {
                reported_status.to_string()
            })
        });
        let requested = FinalizeThreadRecord {
            status: reported_status.to_string(),
            outcome_code: reported_outcome_code,
            result_json: completion.result.clone(),
            error_json: completion.error.clone(),
            artifacts: completion
                .artifacts
                .iter()
                .map(artifact_to_record)
                .collect(),
            final_cost: completion.final_cost.clone(),
            managed_envelope: managed_envelope.clone(),
        };
        let (persisted, effective) = if runtime_callback {
            self.state_store
                .finalize_thread_from_runtime(thread_id, &requested)?
        } else {
            self.state_store
                .finalize_thread_effective(thread_id, &requested)?
        };
        let terminal_status = effective.status.as_str();
        let outcome_code = effective.outcome_code.as_deref();
        self.publish_records(&persisted);
        // Terminal: no queued operator input may survive, and a late enqueue
        // racing this finalize must be refused. Close after the terminal state
        // write (above), so the queue `closed` tombstone and the persist-path
        // running-guard together bound the race from either ordering.
        self.close_live_input(thread_id);

        // Settle any commands still open for this now-terminal thread and wake
        // blocked `commands.wait` callers. This is the primary cooperative path —
        // a runtime self-finalizing over its callback (e.g. a directive cancelled
        // by signal, which never drains its own command) reaches finalization
        // HERE, not through `finalize_thread_inner`.
        self.settle_open_commands(thread_id, terminal_status);

        // Failure-gated stderr surfacing. A subprocess tool's real error
        // (traceback, `logger.error(...)`) rides in `error.stderr`; on the
        // scheduled path the only live signal operators see is the daemon's
        // tracing stream (deploy logs), so emit it there at ERROR level when
        // the run failed. Healthy runs stay silent — no stderr in `error`.
        if matches!(
            ThreadStatus::from_str_lossy(terminal_status),
            Some(ThreadStatus::Failed | ThreadStatus::Killed)
        ) {
            if let Some(stderr_tail) = effective
                .error_json
                .as_ref()
                .and_then(|e| e.get("stderr"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let exit_code = effective
                    .error_json
                    .as_ref()
                    .and_then(|e| e.get("exit_code"))
                    .and_then(Value::as_i64);
                let soft_failure = effective
                    .error_json
                    .as_ref()
                    .and_then(|e| e.get("soft_failure"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                tracing::error!(
                    thread_id,
                    outcome_code = outcome_code.unwrap_or(""),
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
            outcome_code,
            effective.result_json.as_ref(),
        );

        let finalized = self
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {thread_id}"))?;

        // A continuation is a *cut-off* handoff from a RUNNING source (the live
        // `request_continuation` UDS handler), never a terminal-completion edge:
        // a thread that completes — with or without a question — takes the
        // operator follow-up path. So finalize does NOT spawn a successor here.

        // The StateStore strips a runtime envelope when durable stop intent
        // changes its terminal meaning. Feed follow settlement from that same
        // effective record rather than from the superseded process claim.
        let managed_envelope = (effective.status == reported_status)
            .then_some(effective.managed_envelope.clone())
            .flatten();
        self.record_follow_child_terminal(
            &finalized.chain_root_id,
            thread_id,
            terminal_status,
            effective.result_json.as_ref(),
            effective.error_json.as_ref(),
            effective.final_cost.as_ref(),
            managed_envelope,
        );

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
        // No canonical envelope on the generic path (cancel / reconcile / pre-run
        // failure). A follow child that finalizes here degrades to a visible
        // failure envelope; the normal follow terminal carries its envelope via
        // the callback or `finalize_thread_with_managed_envelope`.
        self.finalize_thread_inner(params, None, false)
    }

    /// Like [`finalize_thread`], but carries the runtime's canonical managed
    /// envelope (outputs / warnings / raw cost). The executor-supervised fallback
    /// finalization uses this so a followed child's structured return survives even
    /// when the runtime exited without self-finalizing over the callback.
    pub fn finalize_thread_with_managed_envelope(
        &self,
        params: &ThreadFinalizeParams,
        managed_envelope: Value,
    ) -> Result<ThreadDetail> {
        self.finalize_thread_inner(params, Some(managed_envelope), false)
    }

    /// Settle a link-failure row only if it is atomically proven to remain a
    /// never-launched child. This is the lifecycle wrapper around the StateStore
    /// conditional transition, so successful cleanup still publishes the terminal
    /// event and performs scheduler/command/follow bookkeeping.
    pub fn finalize_created_unattached_if_current(
        &self,
        params: &ThreadFinalizeParams,
    ) -> Result<FinalizeCreatedUnattachedOutcome> {
        let reported_status = normalize_terminal_status(&params.status)?;
        let requested = FinalizeThreadRecord {
            status: reported_status.to_string(),
            outcome_code: params.outcome_code.clone(),
            result_json: params.result.clone(),
            error_json: params.error.clone(),
            artifacts: params.artifacts.iter().map(artifact_to_record).collect(),
            final_cost: params.final_cost.clone(),
            managed_envelope: None,
        };
        match self
            .state_store
            .finalize_created_unattached_if_current(&params.thread_id, &requested)?
        {
            StoreFinalizeCreatedUnattachedOutcome::Finalized {
                persisted,
                effective,
            } => self
                .finish_generic_finalization(params, reported_status, persisted, effective)
                .map(FinalizeCreatedUnattachedOutcome::Finalized),
            StoreFinalizeCreatedUnattachedOutcome::AlreadyTerminal => self
                .get_thread(&params.thread_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "terminal thread missing after conditional finalize: {}",
                        params.thread_id
                    )
                })
                .map(FinalizeCreatedUnattachedOutcome::AlreadyTerminal),
            StoreFinalizeCreatedUnattachedOutcome::NotCurrent {
                process_attached,
                launch_claimed,
                ..
            } => self
                .get_thread(&params.thread_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "thread missing after refused conditional finalize: {}",
                        params.thread_id
                    )
                })
                .map(|thread| FinalizeCreatedUnattachedOutcome::NotCurrent {
                    thread,
                    process_attached,
                    launch_claimed,
                }),
        }
    }

    /// Atomically finalize only if the row remains nonterminal. This is the
    /// fallback-finalizer entry point for runtimes that may concurrently win
    /// through their callback: the StateStore performs the terminal check and
    /// write under one lock, while this layer retains publication and all
    /// terminal postcommit bookkeeping for the winning write.
    pub fn finalize_if_nonterminal(
        &self,
        params: &ThreadFinalizeParams,
    ) -> Result<FinalizeIfNonterminalOutcome> {
        let reported_status = normalize_terminal_status(&params.status)?;
        let requested = FinalizeThreadRecord {
            status: reported_status.to_string(),
            outcome_code: params.outcome_code.clone(),
            result_json: params.result.clone(),
            error_json: params.error.clone(),
            artifacts: params.artifacts.iter().map(artifact_to_record).collect(),
            final_cost: params.final_cost.clone(),
            managed_envelope: None,
        };
        match self
            .state_store
            .finalize_if_nonterminal(&params.thread_id, &requested)?
        {
            StoreFinalizeIfNonterminalOutcome::Finalized {
                persisted,
                effective,
            } => self
                .finish_generic_finalization(params, reported_status, persisted, effective)
                .map(FinalizeIfNonterminalOutcome::Finalized),
            StoreFinalizeIfNonterminalOutcome::AlreadyTerminal { status } => {
                Ok(FinalizeIfNonterminalOutcome::AlreadyTerminal { status })
            }
            StoreFinalizeIfNonterminalOutcome::PreservedForShutdown => {
                Ok(FinalizeIfNonterminalOutcome::PreservedForShutdown)
            }
        }
    }

    fn finalize_thread_inner(
        &self,
        params: &ThreadFinalizeParams,
        managed_envelope: Option<Value>,
        execution_result: bool,
    ) -> Result<ThreadDetail> {
        let reported_status = normalize_terminal_status(&params.status)?;
        let requested = FinalizeThreadRecord {
            status: reported_status.to_string(),
            outcome_code: params.outcome_code.clone(),
            result_json: params.result.clone(),
            error_json: params.error.clone(),
            artifacts: params.artifacts.iter().map(artifact_to_record).collect(),
            final_cost: params.final_cost.clone(),
            managed_envelope: managed_envelope.clone(),
        };
        let (persisted, effective) = if execution_result {
            self.state_store
                .finalize_thread_from_runtime(&params.thread_id, &requested)?
        } else {
            self.state_store
                .finalize_thread_effective(&params.thread_id, &requested)?
        };
        self.finish_generic_finalization(params, reported_status, persisted, effective)
    }

    fn finish_generic_finalization(
        &self,
        params: &ThreadFinalizeParams,
        reported_status: &str,
        persisted: Vec<PersistedEventRecord>,
        effective: FinalizeThreadRecord,
    ) -> Result<ThreadDetail> {
        self.publish_records(&persisted);
        // Terminal: close the live-input entry after the terminal state write
        // (see `finalize_from_completion` for the race rationale).
        self.close_live_input(&params.thread_id);

        let terminal_status = effective.status.as_str();
        self.update_scheduler_fire_on_thread_terminal(
            &params.thread_id,
            terminal_status,
            effective.outcome_code.as_deref(),
            effective.result_json.as_ref(),
        );

        // Settle any commands still open for this now-terminal thread. It will
        // never cooperatively handle them (a force-stopped directive, or a run
        // that finished with a queued cancel), so reject them and wake any
        // `commands.wait` blocked on them — otherwise the await rides to its
        // timeout. Best-effort: a failure here must not fail finalization.
        self.settle_open_commands(&params.thread_id, terminal_status);

        let finalized = self
            .get_thread(&params.thread_id)?
            .ok_or_else(|| anyhow!("thread not found after finalize: {}", params.thread_id))?;

        self.record_follow_child_terminal(
            &finalized.chain_root_id,
            &params.thread_id,
            terminal_status,
            effective.result_json.as_ref(),
            effective.error_json.as_ref(),
            effective.final_cost.as_ref(),
            (effective.status == reported_status)
                .then_some(effective.managed_envelope.clone())
                .flatten(),
        );

        Ok(finalized)
    }

    /// Reject any commands still open for a finalized thread and wake blocked
    /// `commands.wait` callers via the settlement hub. Best-effort — logged, not
    /// raised, so a settlement failure never fails finalization.
    fn settle_open_commands(&self, thread_id: &str, terminal_status: &str) {
        match self
            .state_store
            .settle_open_commands(thread_id, terminal_status)
        {
            Ok(settled) => {
                for record in &settled {
                    crate::command_hub::global().publish(record);
                }
                if !settled.is_empty() {
                    tracing::debug!(
                        thread_id,
                        count = settled.len(),
                        "settled open commands on thread finalize"
                    );
                }
            }
            Err(e) => tracing::warn!(
                thread_id,
                error = %e,
                "failed to settle open commands on thread finalize"
            ),
        }
    }

    /// After a thread finalizes, record its result on any follow waiter awaiting
    /// this child chain. A `continued` finalize is an intermediate link in the
    /// child's OWN continuation chain, not the terminal tail the parent awaits, so
    /// it is skipped. This only stores the canonical envelope and flips the waiter
    /// to `ready`; kicking the parent resume is the follow-resume path's job.
    /// A no-op when no waiter awaits this chain (the common case).
    #[allow(clippy::too_many_arguments)]
    fn record_follow_child_terminal(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        terminal_status: &str,
        result: Option<&Value>,
        error: Option<&Value>,
        final_cost: Option<&ryeos_engine::contracts::FinalCost>,
        managed_envelope: Option<Value>,
    ) {
        if terminal_status == ryeos_state::objects::ThreadStatus::Continued.as_str() {
            return;
        }
        match self
            .state_store
            .get_follow_waiter_by_child_chain(chain_root_id)
        {
            // No waiter awaits this chain — nothing to record (the common,
            // non-follow case). No noise.
            Ok(None) => return,
            Ok(Some(_)) => {}
            Err(e) => {
                tracing::warn!(
                    thread_id,
                    chain_root_id,
                    error = %e,
                    "failed to look up follow waiter for terminal recording",
                );
                return;
            }
        }
        // Only the followed child itself — or a continuation successor of it —
        // settles the follow. The child's chain also carries AUXILIARY runs
        // (launch-time knowledge composition, nested dispatches); the first of
        // those completes in milliseconds, and recording it would resume the
        // parent with the auxiliary's envelope while the child is still
        // launching.
        match self.chain_continuation_lineage_contains(chain_root_id, thread_id) {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    thread_id,
                    chain_root_id,
                    "terminal thread in a followed chain is not in the followed child's \
                     continuation lineage (auxiliary run); waiter untouched",
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    thread_id,
                    chain_root_id,
                    error = %e,
                    "follow lineage check failed; leaving waiter untouched",
                );
                return;
            }
        }
        // Prefer the canonical envelope carried through finalization (preserves
        // outputs/warnings/raw cost). When none is present, this is a cancel /
        // reconcile / pre-run finalize or an old runtime — the stored result
        // must be a visible in-band FAILURE (not a silently-empty success) so
        // the parent resumes into on_error.
        let envelope = match managed_envelope {
            Some(env) => env,
            None => {
                tracing::warn!(
                    thread_id,
                    chain_root_id,
                    "follow child finalized without a canonical envelope; storing a \
                     DEGRADED FAILURE envelope (outputs/warnings unavailable)",
                );
                degraded_follow_envelope(terminal_status, result, error, final_cost)
            }
        };
        if let Err(e) = self.state_store.mark_follow_child_terminal(
            chain_root_id,
            thread_id,
            terminal_status,
            &envelope,
        ) {
            tracing::warn!(
                thread_id,
                chain_root_id,
                error = %e,
                "failed to record follow child terminal result",
            );
        }
    }

    /// Whether `thread_id` lies on the chain's continuation lineage: the chain
    /// root, then `successor_thread_id` links from each `continued` handoff.
    /// A chain also carries auxiliary threads (launch-time knowledge
    /// composition, nested dispatches) — in the chain, but NOT the lineage.
    fn chain_continuation_lineage_contains(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<bool> {
        let mut cursor = chain_root_id.to_string();
        for _ in 0..1024 {
            if cursor == thread_id {
                return Ok(true);
            }
            match self.get_thread(&cursor)? {
                Some(t) => match t.successor_thread_id {
                    Some(next) => cursor = next,
                    None => return Ok(false),
                },
                None => return Ok(false),
            }
        }
        Ok(false)
    }

    /// Recover a `waiting` follow waiter whose child chain already reached a
    /// non-continued terminal but was never recorded — the crash window between
    /// persisting the child's terminal and `record_follow_child_terminal`.
    /// `reconcile` proper cannot catch it (it skips terminal threads), so without
    /// this the parent hangs forever. Synthesizes a DEGRADED FAILURE envelope from
    /// the persisted terminal record (canonical outputs/warnings are gone after a
    /// restart) and flips the waiter to `ready`. Returns the `follow_key` if it
    /// flipped, so the caller can drive the parent resume; `None` if the chain is not
    /// yet terminal or no `waiting` waiter awaits it.
    pub fn recover_terminal_follow_child(
        &self,
        child_chain_root_id: &str,
    ) -> Result<Option<String>> {
        let waiter = match self
            .state_store
            .get_follow_waiter_by_child_chain(child_chain_root_id)?
        {
            Some(w) if w.phase == crate::runtime_db::follow_phase::WAITING => w,
            _ => return Ok(None),
        };
        // The chain tail the parent awaits: walk the continuation lineage from
        // the chain root (the followed child) via successor links to its tip.
        // The chain also carries AUXILIARY threads (launch-time knowledge
        // composition, nested dispatches) whose terminals must never satisfy
        // the recovery — only the child's own lineage settles the follow.
        let mut cursor = child_chain_root_id.to_string();
        let mut tip = None;
        for _ in 0..1024 {
            let Some(t) = self.get_thread(&cursor)? else {
                break;
            };
            if t.status == ryeos_state::objects::ThreadStatus::Continued.as_str() {
                match t.successor_thread_id {
                    Some(next) => {
                        cursor = next;
                        continue;
                    }
                    // A handoff in flight (no successor recorded yet) — not an end.
                    None => break,
                }
            }
            if ryeos_state::objects::ThreadStatus::from_str_lossy(&t.status)
                .is_some_and(|s| s.is_terminal())
            {
                tip = Some(t);
            }
            break;
        }
        let terminal = match tip {
            Some(t) => t,
            // Chain not yet terminal — reconcile's native resume / the finalize kick
            // still own it; nothing to recover here.
            None => return Ok(None),
        };
        let (result, error) = self
            .get_thread_result(&terminal.thread_id)?
            .map(|r| (r.result, r.error))
            .unwrap_or((None, None));
        let envelope =
            degraded_follow_envelope(&terminal.status, result.as_ref(), error.as_ref(), None);
        let flipped = self.state_store.mark_follow_child_terminal(
            child_chain_root_id,
            &terminal.thread_id,
            &terminal.status,
            &envelope,
        )?;
        Ok(flipped.then_some(waiter.follow_key))
    }

    fn update_scheduler_fire_on_thread_terminal(
        &self,
        thread_id: &str,
        terminal_status: &str,
        outcome_code: Option<&str>,
        result: Option<&Value>,
    ) {
        let runtime_gate = self
            .scheduler_runtime_gate
            .read()
            .ok()
            .and_then(|guard| guard.clone());
        let Some(runtime_gate) = runtime_gate else {
            return;
        };
        let Ok(_runtime_guard) = runtime_gate.try_read() else {
            // An exclusive schedule/project/retention mutation owns the state.
            // Do not block synchronous thread finalization; the scheduler's
            // periodic repair sweep will classify this durable terminal thread.
            tracing::debug!(
                thread_id = %thread_id,
                "scheduler: terminal completion hook deferred behind runtime mutation"
            );
            return;
        };
        let db = self
            .scheduler_db
            .read()
            .ok()
            .and_then(|guard| guard.clone());
        let app_root = self.app_root.read().ok().and_then(|guard| guard.clone());
        if let (Some(db), Some(app_root)) = (db, app_root) {
            if let Ok(Some(fire)) = db.find_fire_by_thread(thread_id) {
                let fire_status = ryeos_scheduler::fire_status_for_thread_status(terminal_status);
                let result_outcome = result.map(ryeos_scheduler::classify_result_payload);
                let outcome_str = ryeos_scheduler::fire_outcome_for_terminal(
                    terminal_status,
                    result_outcome.as_ref(),
                    outcome_code,
                );
                let now = lillux::time::timestamp_millis();
                let completed_fired_at = fire.fired_at.unwrap_or(now);

                let updated = ryeos_scheduler::types::FireRecord {
                    status: fire_status.to_string(),
                    outcome: Some(outcome_str.to_string()),
                    fired_at: Some(completed_fired_at),
                    completed_at: Some(now),
                    ..fire
                };
                if let Err(e) =
                    ryeos_scheduler::projection::persist_fire_snapshot(&app_root, &db, &updated)
                {
                    tracing::warn!(
                        thread_id = %thread_id,
                        error = %e,
                        "scheduler: failed to update fire status on thread completion"
                    );
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

    /// The count of operator inputs staged (not yet folded) for a thread, read
    /// from the shared live-input queue. `0` when the queue is unwired (tests) or
    /// the thread has no staged input.
    fn pending_input(&self, thread_id: &str) -> usize {
        self.live_input
            .read()
            .unwrap()
            .as_ref()
            .map(|q| q.pending_len(thread_id))
            .unwrap_or(0)
    }

    /// Compute a thread's follow-lineage fact from a single-thread lookup: the
    /// live waiter (suspended parent / resume successor) first, then — for a
    /// resume successor whose waiter was already cleared — the projected
    /// `graph_follow_resume` continuation edge. Returns `None` for a non-follow
    /// thread. Not used on the list hot path (see `decorate_list_items`, which
    /// batches the waiter read).
    fn follow_fact_for(
        &self,
        thread_id: &str,
        status: &str,
        upstream_thread_id: Option<&str>,
    ) -> Result<Option<FollowFact>> {
        if status == ThreadStatus::Continued.as_str() {
            if let Some(w) = self
                .state_store
                .get_follow_waiter_by_parent_thread(thread_id)?
            {
                return Ok(Some(FollowFact::suspended_parent(&w)));
            }
        }
        if let Some(w) = self.state_store.get_follow_waiter_by_successor(thread_id)? {
            return Ok(Some(FollowFact::resume_successor_live(&w)));
        }
        if let Some(upstream) = upstream_thread_id {
            if self
                .state_store
                .is_follow_resume_successor(upstream, thread_id)?
            {
                return Ok(Some(FollowFact::resume_successor_durable(thread_id)));
            }
        }
        Ok(None)
    }

    /// Decorate a state-store thread projection with daemon-authored execution
    /// facts, follow lineage, and staged-input depth. Kept here (not in
    /// `StateStore`) because kind policy and follow lineage are daemon concerns,
    /// not state-store row data. Single-thread path: for the top-level list use
    /// `decorate_list_items`, which batches the follow-waiter read.
    fn decorate_thread(&self, thread: ThreadDetail) -> Result<ThreadView> {
        let execution = self.execution_facts(&thread.kind);
        let follow = self.follow_fact_for(
            &thread.thread_id,
            &thread.status,
            thread.upstream_thread_id.as_deref(),
        )?;
        let project = thread
            .project_root
            .as_deref()
            .map(Path::new)
            .map(project_summary);
        let pending = self.pending_input(&thread.thread_id);
        Ok(ThreadView {
            thread,
            execution,
            follow,
            project,
            pending,
        })
    }

    /// Decorate a page of list rows. Batches the follow lineage into one
    /// page-scoped, envelope-free waiter projection, so the list path never
    /// issues a per-row runtime_db query. The durable
    /// (waiter-cleared) resume-successor derivation is deliberately NOT done here
    /// — it is a per-row projection query reserved for single-thread inspect;
    /// once the waiter is gone the successor is an ordinary recoverable thread and
    /// its lineage shows in inspect, not the list.
    fn decorate_list_items(&self, items: Vec<ThreadListItem>) -> Result<Vec<ThreadListView>> {
        let thread_ids = items
            .iter()
            .map(|item| item.thread_id.clone())
            .collect::<Vec<_>>();
        let enrichment = self.state_store.thread_list_enrichment(&thread_ids)?;
        Ok(self.decorate_list_items_with_enrichment(items, enrichment))
    }

    fn decorate_list_items_with_enrichment(
        &self,
        items: Vec<ThreadListItem>,
        enrichment: crate::state_store::ThreadListEnrichment,
    ) -> Vec<ThreadListView> {
        let crate::state_store::ThreadListEnrichment {
            follow_waiters: waiters,
            mut facets,
            mut current_graph_nodes,
        } = enrichment;
        let mut by_parent: std::collections::HashMap<
            &str,
            &crate::runtime_db::FollowWaiterSummary,
        > = std::collections::HashMap::new();
        let mut by_successor: std::collections::HashMap<
            &str,
            &crate::runtime_db::FollowWaiterSummary,
        > = std::collections::HashMap::new();
        for w in &waiters {
            by_parent.insert(w.parent_thread_id.as_str(), w);
            if let Some(succ) = &w.parent_successor_thread_id {
                by_successor.insert(succ.as_str(), w);
            }
        }
        items
            .into_iter()
            .map(|item| {
                let execution = self.execution_facts(&item.kind);
                let follow = if item.status == ThreadStatus::Continued.as_str() {
                    by_parent
                        .get(item.thread_id.as_str())
                        .map(|w| FollowFact::suspended_parent_summary(w))
                } else {
                    None
                }
                .or_else(|| {
                    by_successor
                        .get(item.thread_id.as_str())
                        .map(|w| FollowFact::resume_successor_live_summary(w))
                });
                let project = item
                    .project_root
                    .as_deref()
                    .map(Path::new)
                    .map(project_summary);
                let pending = self.pending_input(&item.thread_id);
                let facets = facets
                    .remove(&item.thread_id)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                let current_node = current_graph_nodes
                    .remove(&item.thread_id)
                    .map(|(node, step)| CurrentNode { node, step });
                ThreadListView {
                    item,
                    execution,
                    follow,
                    project,
                    pending,
                    facets,
                    current_node,
                }
            })
            .collect()
    }

    fn thread_list_item_matches_filter(
        item: &ThreadListItem,
        filter: &ryeos_state::queries::ThreadListFilter,
        facets: &std::collections::HashMap<String, Vec<(String, String)>>,
    ) -> bool {
        if let Some(principal) = &filter.principal {
            if item.requested_by.as_deref() != Some(principal.as_str()) {
                return false;
            }
        }
        if let Some(status) = &filter.status {
            if !item.status.contains(status) {
                return false;
            }
        }
        if let Some(kind) = &filter.kind {
            if !item.kind.contains(kind) {
                return false;
            }
        }
        if let Some(requested_by) = &filter.requested_by {
            if !item
                .requested_by
                .as_deref()
                .is_some_and(|value| value.contains(requested_by))
            {
                return false;
            }
        }
        if filter
            .exclude_item_prefixes
            .iter()
            .any(|prefix| item.item_ref.starts_with(prefix))
        {
            return false;
        }
        if let Some(project_root) = &filter.project_root {
            if item.project_root.as_deref().map(Path::new) != Some(project_root.as_path()) {
                return false;
            }
        }
        if let Some((key, value)) = &filter.facet {
            let has_facet = facets
                .get(&item.thread_id)
                .is_some_and(|rows| rows.iter().any(|(k, v)| k == key && v == value));
            if !has_facet {
                return false;
            }
        }
        true
    }

    /// `get_thread` for client-facing callers: the projection plus its
    /// daemon-authored execution facts, follow lineage, and staged-input depth.
    pub fn get_thread_view(&self, thread_id: &str) -> Result<Option<ThreadView>> {
        self.get_thread(thread_id)?
            .map(|t| self.decorate_thread(t))
            .transpose()
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        self.state_store.get_thread_result(thread_id)
    }

    pub fn get_thread_terminal_authority(
        &self,
        thread_id: &str,
    ) -> Result<Option<crate::state_store::ThreadTerminalAuthority>> {
        self.state_store.get_thread_terminal_authority(thread_id)
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
    pub fn request_continuation_with_events(
        &self,
        params: &ThreadContinuationParams,
        successor_thread_id: &str,
        expected_resume_context: &crate::launch_metadata::ResumeContext,
        successor_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<ThreadContinuationResult> {
        validate_thread_id_format(successor_thread_id)?;
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
        let successor_record = NewThreadRecord {
            thread_id: successor_thread_id.to_string(),
            chain_root_id: source.chain_root_id.clone(),
            kind: source.kind.clone(),
            item_ref: source.item_ref.clone(),
            executor_ref: source.executor_ref.clone(),
            launch_mode: source.launch_mode.clone(),
            current_site_id: source.current_site_id.clone(),
            origin_site_id: source.origin_site_id.clone(),
            upstream_thread_id: Some(source.thread_id.clone()),
            requested_by: source.requested_by.clone(),
            project_root: source.project_root.as_ref().map(PathBuf::from),
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: None,
        };

        // One atomic op under the state store's write permit + lock: re-verify the
        // source is still `running`, require its captured `ResumeContext`, create
        // the successor, seed the successor's launch identity (runtime-db writes
        // first), then settle the source `continued`. A race or seed failure
        // aborts with the source still running — never `continued` behind an
        // unlaunchable successor.
        let persisted = self.state_store.create_machine_continuation_with_events(
            &successor_record,
            &source.thread_id,
            &source.chain_root_id,
            params.reason.as_deref(),
            expected_resume_context,
            successor_launch_metadata,
            initial_events,
        )?;

        let successor = self
            .get_thread(successor_thread_id)?
            .ok_or_else(|| {
                anyhow!("successor thread missing after creation: {successor_thread_id}")
            })?;
        self.publish_records(&persisted);

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
        completion: &ryeos_runtime::TerminalCompletion,
    ) -> Result<()> {
        validate_continued_completion(completion)?;
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

    /// Record one portable cross-chain parent→child edge exactly once and
    /// publish it only after the signed authoritative append succeeds.
    pub fn append_child_thread_spawned_once(
        &self,
        chain_root_id: &str,
        parent_thread_id: &str,
        child_thread_id: &str,
        payload: Value,
    ) -> Result<ChildLineageAppendOutcome> {
        let result = self.state_store.append_child_thread_spawned_once(
            chain_root_id,
            parent_thread_id,
            child_thread_id,
            payload,
        )?;
        if result.outcome == ChildLineageAppendOutcome::Appended {
            self.publish_records(&result.persisted);
        }
        Ok(result.outcome)
    }

    pub fn list_threads(&self, limit: usize) -> Result<Value> {
        let threads = self.decorate_list_items(self.state_store.list_threads(limit)?)?;
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
        self.decorate_list_items(
            self.state_store
                .list_threads_filtered(limit, filter_principal)?,
        )
    }

    /// As [`Self::list_thread_views_filtered`] but with an explicit sort — the
    /// watch console requests [`ryeos_state::queries::ThreadSort::Watch`]
    /// (active-first, newest) while the default list/CLI order is unchanged.
    pub fn list_thread_views_sorted(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
        sort: ryeos_state::queries::ThreadSort,
    ) -> Result<Vec<ThreadListView>> {
        self.decorate_list_items(self.state_store.list_threads_sorted(
            limit,
            filter_principal,
            sort,
        )?)
    }

    /// As [`Self::list_thread_views_sorted`] but with the full optional filter
    /// set (status / kind / requested_by) the operator dashboard narrows by.
    pub fn list_thread_views_query(
        &self,
        limit: usize,
        filter: &ryeos_state::queries::ThreadListFilter,
        sort: ryeos_state::queries::ThreadSort,
    ) -> Result<Vec<ThreadListView>> {
        let mut items = self.state_store.list_threads_query(limit, filter, sort)?;
        if filter.active_only || filter.project_root.is_some() {
            let crate::state_store::FollowParentListSnapshot { waiters, parents } =
                self.state_store.follow_parent_list_snapshot()?;
            let mut enrichment_ids = items
                .iter()
                .map(|item| item.thread_id.clone())
                .collect::<Vec<_>>();
            enrichment_ids.extend(parents.iter().map(|item| item.thread_id.clone()));
            enrichment_ids.sort();
            enrichment_ids.dedup();
            let enrichment = self
                .state_store
                .thread_list_enrichment_with_waiters(&enrichment_ids, waiters)?;
            let mut seen = items
                .iter()
                .map(|item| item.thread_id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let mut suspended = std::collections::BTreeSet::new();
            for item in parents {
                if item.status != ThreadStatus::Continued.as_str()
                    || !Self::thread_list_item_matches_filter(&item, filter, &enrichment.facets)
                {
                    continue;
                }
                // A matching continued parent may already be in the SQL page,
                // but the live waiter still makes it effectively active for
                // Watch ordering.
                suspended.insert(item.thread_id.clone());
                if seen.insert(item.thread_id.clone()) {
                    items.push(item);
                }
            }
            if sort == ryeos_state::queries::ThreadSort::Watch {
                items.sort_by(|a, b| {
                    let a_terminal = ryeos_state::objects::ThreadStatus::from_str_lossy(&a.status)
                        .is_some_and(|status| status.is_terminal())
                        && !suspended.contains(&a.thread_id);
                    let b_terminal = ryeos_state::objects::ThreadStatus::from_str_lossy(&b.status)
                        .is_some_and(|status| status.is_terminal())
                        && !suspended.contains(&b.thread_id);
                    a_terminal
                        .cmp(&b_terminal)
                        .then_with(|| b.created_at.cmp(&a.created_at))
                });
            } else {
                sort_thread_list_items(&mut items, sort);
            }
            items.truncate(limit);
            return Ok(self.decorate_list_items_with_enrichment(items, enrichment));
        }
        self.decorate_list_items(items)
    }

    /// Resolve any selected thread into its bounded execution tree, decorating
    /// every structural row through the same batched follow/current-node path
    /// as the history dashboard.
    pub fn execution_tree(
        &self,
        selected_thread_id: &str,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<ExecutionTreeResult> {
        let page = self
            .state_store
            .execution_tree(selected_thread_id, max_depth, max_nodes)?;
        let positions = page
            .items
            .iter()
            .map(|item| ExecutionTreePosition {
                parent_thread_id: item.tree_parent_thread_id.clone(),
                relation: item.relation.clone(),
                depth: item.depth,
                has_children: item.has_children,
            })
            .collect::<Vec<_>>();
        let items = page
            .items
            .into_iter()
            .map(|item| item.item)
            .collect::<Vec<_>>();
        let decorated = self.decorate_list_items(items)?;
        let threads = decorated
            .into_iter()
            .zip(positions)
            .map(|(thread, tree)| ExecutionTreeView { thread, tree })
            .collect::<Vec<_>>();
        let root_thread_id = threads.first().map(|row| row.thread.item.thread_id.clone());
        Ok(ExecutionTreeResult {
            root_thread_id,
            threads,
            truncated: page.truncated,
        })
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

    /// As [`Self::list_threads_filtered`] but with an explicit sort — the
    /// public `threads.list` exposes `newest` ("what just ran"; the limit
    /// truncates, so oldest-first + limit returns the OLDEST rows) and
    /// `watch` alongside the default oldest-first order.
    pub fn list_threads_filtered_sorted(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
        sort: ryeos_state::queries::ThreadSort,
    ) -> Result<Value> {
        let threads = self.list_thread_views_sorted(limit, filter_principal, sort)?;
        Ok(json!({ "threads": threads, "next_cursor": null }))
    }

    /// As [`Self::list_threads_filtered_sorted`] but also narrows to a cohort
    /// facet (`key == value`, e.g. `fleet=<run id>`) when one is given — the
    /// fleet-membership query, on top of the owner-scope principal.
    pub fn list_threads_filtered_sorted_facet(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
        facet: Option<(String, String)>,
        sort: ryeos_state::queries::ThreadSort,
    ) -> Result<Value> {
        let filter = ryeos_state::queries::ThreadListFilter {
            principal: filter_principal.map(str::to_string),
            facet,
            ..Default::default()
        };
        let threads = self.list_thread_views_query(limit, &filter, sort)?;
        Ok(json!({ "threads": threads, "next_cursor": null }))
    }

    pub fn list_children(&self, thread_id: &str) -> Result<Vec<ThreadView>> {
        self.state_store
            .list_thread_children(thread_id)?
            .into_iter()
            .map(|thread| self.decorate_thread(thread))
            .collect()
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
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(ThreadChainResult {
            threads,
            edges: self.state_store.list_chain_edges(&thread.chain_root_id)?,
        }))
    }
}

/// Build the native managed-dispatch envelope shape
/// (`{success, status, result, outputs, warnings, cost}`) from a runtime's RAW
/// terminal fields, so a stored follow result classifies byte-for-byte like a live
/// child dispatch. `raw_cost` is the runtime's own cost object (not the lossy
/// re-serialized `FinalCost`). A failure carries its cause in `result` (the native
/// envelope has no separate error field).
pub fn managed_runtime_envelope(
    status: &str,
    result: Option<&Value>,
    error: Option<&Value>,
    raw_cost: Option<&Value>,
    outputs: &Value,
    warnings: &[String],
) -> Value {
    let success = status == ryeos_state::objects::ThreadStatus::Completed.as_str();
    let payload = result.or(error).cloned().unwrap_or(Value::Null);
    json!({
        "success": success,
        "status": status,
        "result": payload,
        "outputs": outputs,
        "warnings": warnings,
        "cost": raw_cost.cloned(),
    })
}

/// Marker code stored in a degraded follow envelope's result + warnings so the
/// loss of the canonical envelope is visible in-band, not just in logs.
pub const DEGRADED_FOLLOW_ENVELOPE_CODE: &str = "degraded_follow_child_terminal_envelope";

/// Build a DEGRADED FAILURE envelope for a follow child that finalized without a
/// canonical runtime envelope (outputs/warnings unavailable — e.g. a cancel /
/// reconcile / pre-run finalize, or an old runtime). Deliberately `success:false`
/// so the parent's resume takes its child-failure / on_error path rather than
/// resuming with silently-missing outputs. Used ONLY when a real waiter would
/// otherwise consume an unmarked result.
fn degraded_follow_envelope(
    child_status: &str,
    child_result: Option<&Value>,
    child_error: Option<&Value>,
    final_cost: Option<&ryeos_engine::contracts::FinalCost>,
) -> Value {
    let status = ryeos_runtime::envelope::RuntimeResultStatus::Failed;
    let raw_cost = final_cost.map(|c| {
        json!({
            "input_tokens": c.input_tokens,
            "output_tokens": c.output_tokens,
            "total_usd": c.spend,
        })
    });
    json!({
        "success": false,
        "status": status,
        "result": {
            "code": DEGRADED_FOLLOW_ENVELOPE_CODE,
            "message": "follow child finalized without a canonical runtime envelope; \
                        outputs/warnings unavailable",
            "child_status": child_status,
            "child_result": child_result.or(child_error).cloned().unwrap_or(Value::Null),
        },
        "outputs": Value::Null,
        "warnings": [DEGRADED_FOLLOW_ENVELOPE_CODE],
        "cost": raw_cost,
    })
}

fn artifact_to_record(artifact: &ExecutionArtifact) -> NewArtifactRecord {
    NewArtifactRecord {
        artifact_type: artifact.artifact_type.clone(),
        uri: artifact.uri.clone(),
        content_hash: artifact.content_hash.clone(),
        metadata: artifact.metadata.clone(),
    }
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
    pub node_history_policy: &'a ResolvedNodeThreadHistoryPolicy,
    pub site_id: &'a str,
    pub project_path: &'a Path,
    pub item_ref: &'a str,
    pub ref_bindings: BTreeMap<String, String>,
    pub launch_mode: &'a str,
    pub parameters: Value,
    /// Exact acting principal identifier. Root execution never invents a local
    /// pseudo-principal when caller authority is absent.
    pub requested_by: String,
    pub usage_subject: Option<UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
    pub caller_scopes: Vec<String>,
    pub validate_only: bool,
    /// True only when the caller will mint a new chain root. Continuations and
    /// existing-row relaunches inherit captured root policy and must not
    /// reinterpret current authored history content.
    pub creates_chain_root: bool,
}

pub fn resolve_root_execution(
    params: ResolveRootExecutionParams<'_>,
) -> Result<ResolvedExecutionRequest> {
    let ResolveRootExecutionParams {
        engine,
        node_history_policy,
        site_id,
        project_path,
        item_ref,
        ref_bindings,
        launch_mode,
        parameters,
        requested_by,
        usage_subject,
        usage_subject_asserted_by,
        caller_scopes,
        validate_only,
        creates_chain_root,
    } = params;
    validate_principal_identifier("root execution acting principal", &requested_by)?;
    let project_path = project_path
        .canonicalize()
        .with_context(|| format!("canonicalize execution project {}", project_path.display()))?;

    let canonical_ref =
        CanonicalRef::parse(item_ref).map_err(|e| anyhow!("invalid item ref: {e}"))?;

    validate_launch_mode(launch_mode)?;

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: requested_by.clone(),
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

    let thread_kind = engine
        .kinds
        .get(&resolved.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .map(|profile| profile.name.clone())
        .ok_or_else(|| {
            anyhow!(
                "root item `{}` has no execution thread profile in its verified kind schema",
                resolved.canonical_ref
            )
        })?;

    let root_admission = creates_chain_root
        .then(|| {
            let verified = engine
                .verify(&plan_ctx, resolved.clone())
                .map_err(|error| anyhow!("root admission verification failed: {error}"))?;
            admit_verified_root_execution(
                engine,
                &plan_ctx,
                verified,
                node_history_policy,
                thread_kind.clone(),
                ref_bindings.clone(),
                usage_subject.clone(),
                usage_subject_asserted_by.clone(),
            )
        })
        .transpose()?;

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

    let root_raw_content_digest = resolved.raw_content_digest.clone();
    Ok(ResolvedExecutionRequest {
        kind: thread_kind,
        item_ref: item_ref.to_string(),
        executor_ref,
        launch_mode: launch_mode.to_string(),
        current_site_id: site_id.to_string(),
        origin_site_id: site_id.to_string(),
        target_site_id: None,
        requested_by: Some(requested_by),
        usage_subject,
        usage_subject_asserted_by,
        parameters,
        ref_bindings,
        resolved_item: resolved,
        root_raw_content_digest,
        plan_context: plan_ctx,
        root_admission,
    })
}

/// Resolve a concrete history launch contract from the same verified composed
/// value execution will receive. This must run before root persistence.
pub fn resolve_thread_history_policy(
    engine: &Engine,
    plan_context: &PlanContext,
    resolved_item: &ResolvedItem,
    node_history_policy: &ResolvedNodeThreadHistoryPolicy,
) -> Result<ResolvedThreadHistoryPolicy> {
    let verified = engine
        .verify(plan_context, resolved_item.clone())
        .map_err(|error| anyhow!("history-policy item verification failed: {error}"))?;
    let project_root = match &plan_context.project_context {
        ProjectContext::LocalPath { path } => Some(path.as_path()),
        ProjectContext::SnapshotHash { .. } | ProjectContext::ProjectRef { .. } => {
            resolved_item.materialized_project_root.as_deref()
        }
        ProjectContext::None => None,
    };
    let roots = engine.resolution_roots(project_root.map(Path::to_path_buf));
    let parsers = engine
        .effective_parser_dispatcher(project_root)
        .map_err(|error| anyhow!("history-policy parser resolution failed: {error}"))?;
    let resolution = ryeos_engine::resolution::run_resolution_pipeline(
        &resolved_item.canonical_ref,
        &engine.kinds,
        &parsers,
        &roots,
        &engine.trust_store,
        &engine.composers,
    )
    .map_err(|error| anyhow!("history-policy composition failed: {error}"))?;
    Ok(
        ryeos_engine::history_policy::resolve_launch_policy_from_resolution(
            &verified,
            &resolution,
            &engine.kinds,
            node_history_policy,
        )
        .map_err(|error| anyhow!("history-policy resolution failed: {error}"))?
        .history,
    )
}

/// Build the immutable root contract from one already-verified subject. The
/// composition pass is bound to the verified root digest by
/// `resolve_launch_policy_from_resolution`; an in-place edit between verify
/// and compose therefore fails instead of producing a mixed admission.
pub fn admit_verified_root_execution(
    engine: &Engine,
    plan_context: &PlanContext,
    verified_subject: VerifiedItem,
    node_history_policy: &ResolvedNodeThreadHistoryPolicy,
    thread_profile: String,
    ref_bindings: BTreeMap<String, String>,
    usage_subject: Option<UsageSubject>,
    usage_subject_asserted_by: Option<String>,
) -> Result<RootExecutionAdmission> {
    validate_principal_identifier(
        "root admission planning principal",
        plan_principal_identifier(plan_context),
    )?;
    // A public `VerifiedItem` is evidence, not an admission capability. Run the
    // verifier again here and carry only the verifier's result into the sealed
    // token so fabricated provenance can never cross this boundary.
    let verified_subject = engine
        .verify(plan_context, verified_subject.resolved)
        .map_err(|error| anyhow!("root admission item verification failed: {error}"))?;
    let project_root = match &plan_context.project_context {
        ProjectContext::LocalPath { path } => Some(path.as_path()),
        ProjectContext::SnapshotHash { .. } | ProjectContext::ProjectRef { .. } => verified_subject
            .resolved
            .materialized_project_root
            .as_deref(),
        ProjectContext::None => None,
    };
    let roots = engine.resolution_roots(project_root.map(Path::to_path_buf));
    let parsers = engine
        .effective_parser_dispatcher(project_root)
        .map_err(|error| anyhow!("history-policy parser resolution failed: {error}"))?;
    let resolution = ryeos_engine::resolution::run_resolution_pipeline(
        &verified_subject.resolved.canonical_ref,
        &engine.kinds,
        &parsers,
        &roots,
        &engine.trust_store,
        &engine.composers,
    )
    .map_err(|error| anyhow!("history-policy composition failed: {error}"))?;
    let history = ryeos_engine::history_policy::resolve_launch_policy_from_resolution(
        &verified_subject,
        &resolution,
        &engine.kinds,
        node_history_policy,
    )
    .map_err(|error| anyhow!("history-policy resolution failed: {error}"))?
    .history;
    let admission = RootExecutionAdmission {
        verified_subject,
        resolution_output: resolution,
        plan_context: plan_context.clone(),
        thread_profile,
        usage_subject,
        usage_subject_asserted_by,
        ref_bindings,
        captured_history_policy: capture_thread_history_policy(&history)?,
        resolved_history_policy: history,
    };
    admission.ensure_matches_subject(
        engine,
        &admission.verified_subject,
        &admission.thread_profile,
    )?;
    Ok(admission)
}

/// Resolve and capture policy for a non-execution root whose authoritative
/// subject is still a verified item (for example a UI seat presenting a
/// surface). This uses the same kind/composition/trust pipeline as execution;
/// callers never synthesize Durable policy or branch on the subject kind.
pub fn admit_non_execution_root(
    engine: &Engine,
    node_history_policy: &ResolvedNodeThreadHistoryPolicy,
    item_ref: &str,
    project_path: &Path,
    requested_by: &str,
    caller_scopes: Vec<String>,
    current_site_id: &str,
    thread_profile: String,
) -> Result<NonExecutionRootAdmission> {
    validate_principal_identifier("non-execution root acting principal", requested_by)?;
    let canonical_ref = CanonicalRef::parse(item_ref)
        .map_err(|error| anyhow!("invalid root policy item ref `{item_ref}`: {error}"))?;
    let plan_context = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: requested_by.to_string(),
            scopes: caller_scopes,
        }),
        project_context: ProjectContext::LocalPath {
            path: project_path.canonicalize().with_context(|| {
                format!(
                    "canonicalize non-execution root project {}",
                    project_path.display()
                )
            })?,
        },
        current_site_id: current_site_id.to_string(),
        origin_site_id: current_site_id.to_string(),
        execution_hints: ExecutionHints::default(),
        validate_only: false,
    };
    let resolved = engine
        .resolve(&plan_context, &canonical_ref)
        .map_err(|error| anyhow!("root policy item resolution failed: {error}"))?;
    let verified_subject = engine
        .verify(&plan_context, resolved.clone())
        .map_err(|error| anyhow!("root policy item verification failed: {error}"))?;
    let resolved_history_policy =
        resolve_thread_history_policy(engine, &plan_context, &resolved, node_history_policy)?;
    let admission = NonExecutionRootAdmission {
        verified_subject,
        plan_context,
        thread_profile,
        captured_history_policy: capture_thread_history_policy(&resolved_history_policy)?,
        resolved_history_policy,
    };
    admission.validate_for_persistence(engine)?;
    Ok(admission)
}

/// Trust-verified root admission captured before any thread row exists.
#[derive(Debug, Clone)]
pub struct PreflightRootExecution {
    pub root_admission: RootExecutionAdmission,
}

/// Kind-agnostic existence + trust gate for accepted/background launch.
///
/// Resolves the root item and verifies its trust, returning one indivisible
/// subject-and-history contract before a thread id is minted.
/// Unlike [`resolve_root_execution`]
/// (the terminal-only resolver used by the tool subprocess path), this does
/// not demand `metadata.executor_id` or build a plan — which executor runs
/// (terminal subprocess vs runtime-registry runtime) is decided by
/// `dispatch::dispatch`. Route-specific pre-thread-creation checks
/// (terminal executor_id, direct-runtime caps) live in
/// `ryeos_executor::dispatch::preflight_root_dispatch`.
///
/// Fails fast — before any thread_id is minted — on invalid refs and trust
/// violations.
pub fn preflight_root_execution(
    params: ResolveRootExecutionParams<'_>,
) -> Result<PreflightRootExecution> {
    let ResolveRootExecutionParams {
        engine,
        node_history_policy,
        site_id,
        project_path,
        item_ref,
        ref_bindings,
        launch_mode,
        requested_by,
        usage_subject,
        usage_subject_asserted_by,
        caller_scopes,
        validate_only,
        ..
    } = params;
    validate_principal_identifier("root execution acting principal", &requested_by)?;
    let project_path = project_path
        .canonicalize()
        .with_context(|| format!("canonicalize execution project {}", project_path.display()))?;

    let canonical_ref =
        CanonicalRef::parse(item_ref).map_err(|e| anyhow!("invalid item ref: {e}"))?;

    validate_launch_mode(launch_mode)?;

    let root_executable = engine
        .kinds
        .get(&canonical_ref.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .is_some_and(|profile| profile.root_executable);
    if !root_executable {
        bail!(
            "root item `{item_ref}` has no root-executable thread profile in its verified kind schema"
        );
    }

    let plan_ctx = PlanContext {
        requested_by: EffectivePrincipal::Local(Principal {
            fingerprint: requested_by,
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

    let thread_profile = engine
        .kinds
        .get(&verified.resolved.kind)
        .and_then(|schema| schema.execution())
        .and_then(|execution| execution.thread_profile.as_ref())
        .map(|profile| profile.name.clone())
        .ok_or_else(|| {
            anyhow!(
                "verified root `{}` has no execution thread profile",
                verified.resolved.canonical_ref
            )
        })?;
    let root_admission = admit_verified_root_execution(
        engine,
        &plan_ctx,
        verified.clone(),
        node_history_policy,
        thread_profile,
        ref_bindings,
        usage_subject,
        usage_subject_asserted_by,
    )?;
    Ok(PreflightRootExecution { root_admission })
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
    pub process_identity: crate::process::ExecutionProcessIdentity,
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

/// A verified, fully built item plan retained from callback credential minting
/// through spawn. Its timeout comes from the exact plan the engine executes, so
/// credential lifetime cannot drift from a later plan rebuild.
pub struct PreparedItemPlan {
    plan: ExecutionPlan,
    pub timeout_secs: u64,
}

pub fn prepare_item_plan(
    engine: &Engine,
    resolved: &ResolvedExecutionRequest,
) -> Result<PreparedItemPlan> {
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
    let timeout_secs = match plan.nodes.first() {
        Some(ryeos_engine::contracts::PlanNode::DispatchSubprocess { spec, .. }) => {
            spec.timeout_secs
        }
        Some(ryeos_engine::contracts::PlanNode::Complete { .. }) => {
            bail!("item plan entrypoint is complete, not a subprocess")
        }
        None => bail!("item plan is empty"),
    };
    Ok(PreparedItemPlan { plan, timeout_secs })
}

/// Run the prepared engine plan's spawn phase.
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
    /// Exact verified plan used to derive callback credential lifetime.
    pub prepared_plan: PreparedItemPlan,
    pub thread_id: &'a str,
    pub chain_root_id: &'a str,
    pub vault_bindings: std::collections::HashMap<String, String>,
    /// Exact signed-protocol environment, with each injection retaining its
    /// typed vocabulary source through final composition.
    pub protocol_env_bindings: Vec<EnvBinding>,
    pub roots: DaemonRootEnv,
    pub sandbox: Arc<ryeos_engine::sandbox::SandboxRuntime>,
    pub sandbox_project_authority: ryeos_engine::sandbox::SandboxProjectAuthority,
    /// Exact daemon socket requested by the verified callback channel, or
    /// `None` for a callback-free launch.
    pub sandbox_daemon_socket_path: Option<&'a std::path::Path>,
    pub thread_state_dir: Option<&'a std::path::Path>,
    pub is_resume: bool,
    pub original_snapshot_hash: Option<&'a str>,
    /// Pushed-head identity of the spawn's root provenance (see
    /// `launch_metadata::OriginalPushedHeadRef::from_provenance`).
    /// `None` for live-fs spawns and borrowed children.
    pub original_pushed_head_ref: Option<&'a crate::launch_metadata::OriginalPushedHeadRef>,
    /// The spawn's deliberate state-root override
    /// (`provenance.state_root_override()`), persisted on the resume
    /// context so a resumed run keeps the same state/callback anchor.
    pub state_root: Option<&'a std::path::Path>,
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
        prepared_plan,
        thread_id,
        chain_root_id,
        vault_bindings,
        protocol_env_bindings,
        roots,
        sandbox,
        sandbox_project_authority,
        sandbox_daemon_socket_path,
        thread_state_dir,
        is_resume,
        original_snapshot_hash,
        original_pushed_head_ref,
        state_root,
    } = params;
    let app_root = roots
        .app_root
        .as_deref()
        .map(std::path::PathBuf::from)
        .context("spawn roots missing RYEOS_APP_ROOT")?;
    // vault_bindings: user-provided secret/capability env vars.
    // protocol_env_bindings: the verified terminator protocol's exact signed
    // env contract. Values are produced from daemon-owned launch facts by the
    // runner, never inherited from the daemon's process environment.
    let mut plan = prepared_plan.plan;

    // Compose every subprocess node from allowlisted parent env, daemon roots,
    // declared secrets, engine-plan bindings, and the verified terminator
    // protocol's exact typed injections. No callback key is manufactured here;
    // callback-free protocols therefore stay callback-free through `env_clear`.
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
                    let ckpt = ts_dir.join(crate::launch_metadata::CHECKPOINTS_SUBDIR);
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

            builder = builder.with_typed_bindings(protocol_env_bindings.iter().cloned())?;

            if spec.execution.native_resume.is_some() {
                if let Some(resume_bindings) = resume_env_for_first_native_resume.take() {
                    builder = builder.with_typed_bindings(resume_bindings)?;
                }
            }

            spec.env = builder.build().into_iter().collect();
            spec.env_sources.clear();
        }
    }

    let sandbox_project_root = match &resolved.plan_context.project_context {
        ryeos_engine::contracts::ProjectContext::LocalPath { path } => Some(path.clone()),
        _ => None,
    };
    let sandbox_resolution_roots = engine.resolution_roots(sandbox_project_root);
    let sandbox_bundle_roots = sandbox_resolution_roots
        .ordered
        .iter()
        .filter(|root| root.space == ryeos_engine::contracts::ItemSpace::Bundle)
        .filter_map(|root| root.ai_root.parent().map(std::path::Path::to_path_buf))
        .collect();
    let sandbox_node_trusted_keys_dir = app_root
        .join(ryeos_engine::AI_DIR)
        .join("config/keys/trusted");
    let sandbox_verified_code = plan
        .nodes
        .iter()
        .filter_map(|node| match node {
            ryeos_engine::contracts::PlanNode::DispatchSubprocess {
                tool_path: Some(source_path),
                ..
            } => Some(ryeos_engine::sandbox::SandboxVerifiedCode {
                source_path: source_path.clone(),
                content_hash: resolved.resolved_item.content_hash.clone(),
            }),
            _ => None,
        })
        .collect();
    let engine_ctx = EngineContext {
        app_root,
        sandbox,
        sandbox_project_authority,
        sandbox_state_root: state_root.map(std::path::Path::to_path_buf),
        sandbox_checkpoint_dir: allocated_checkpoint_dir.clone(),
        sandbox_daemon_socket_path: sandbox_daemon_socket_path.map(std::path::Path::to_path_buf),
        sandbox_bundle_roots,
        sandbox_node_trusted_keys_dir: Some(sandbox_node_trusted_keys_dir),
        sandbox_verified_code,
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
                ref_bindings: resolved.ref_bindings.clone(),
                launch_mode: resolved.launch_mode.clone(),
                parameters: resolved.parameters.clone(),
                project_context: resolved.plan_context.project_context.clone(),
                original_snapshot_hash: original_snapshot_hash.map(str::to_string),
                original_pushed_head_ref: original_pushed_head_ref.cloned(),
                state_root: state_root.map(std::path::Path::to_path_buf),
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
    let process_identity = match crate::process::capture_execution_process_identity(
        spawned.pid as i64,
        Some(spawned.pgid),
    ) {
        Ok(identity) => identity,
        Err(
            target_error @ crate::process::ExecutionProcessIdentityCaptureError::TargetAlreadyDeadOrStale,
        ) => {
            // Bubblewrap can report a very short command and reap it before the
            // daemon captures its birth identity. Lillux still retains the
            // wrapper/group leader and owns its wait/output path. Use that exact
            // leader as the durable control target rather than turning a
            // successful fast command into a spawn failure. The reported child
            // PID remains internal accounting in SpawnedExecution.
            crate::process::capture_execution_process_identity(
                spawned.pgid,
                Some(spawned.pgid),
            )
            .with_context(|| {
                format!(
                    "capture spawned target identity ({target_error}); capture retained wrapper identity"
                )
            })?
        }
        Err(error) => return Err(error).context("capture spawned target identity"),
    };
    let durable_pid = u32::try_from(process_identity.target_pid)
        .context("durable spawned target PID exceeds u32")?;

    Ok(SpawnedItem {
        pid: durable_pid,
        pgid: spawned.pgid,
        process_identity,
        launch_metadata,
        spawned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_principal_identifier_is_structured_but_scheme_agnostic() {
        assert!(validate_principal_identifier("principal", "session:session-1").is_ok());
        assert!(
            validate_principal_identifier("principal", &format!("fp:{}", "ab".repeat(32))).is_ok()
        );
        assert!(validate_principal_identifier("principal", "actor:agent-1").is_ok());

        for invalid in [
            "",
            " session:value",
            "session:value ",
            "session:",
            "Session:value",
            "session",
            "session:\nvalue",
        ] {
            assert!(
                validate_principal_identifier("principal", invalid).is_err(),
                "accepted invalid principal identifier {invalid:?}"
            );
        }
    }

    #[test]
    fn engine_and_state_history_duration_domains_match() {
        assert_eq!(
            ryeos_engine::history_policy::MAX_TERMINAL_DURATION_SECONDS,
            ryeos_state::MAX_TERMINAL_DURATION_SECONDS
        );
    }

    #[test]
    fn capture_history_policy_maps_engine_types_without_string_roundtrip() {
        let policy = ResolvedThreadHistoryPolicy {
            retention: ryeos_engine::history_policy::ThreadHistoryRetention::TerminalFor {
                seconds: 60,
            },
            canonical_item_ref: "directive:test".to_string(),
            item_content_hash: "11".repeat(32),
            item_signer_fingerprint: Some("22".repeat(32)),
            item_trust_class: TrustClass::Trusted,
            kind_schema_content_hash: "33".repeat(32),
            source: PolicyProvenance::ItemAuthored {
                composed_path: "history".to_string(),
                requested_seconds: 30,
                effective_trust_class: ResolutionTrustClass::TrustedProject,
                minimum_clamp: Some(ryeos_engine::history_policy::ThreadHistoryMinimumClamp {
                    requested_seconds: 30,
                    minimum_seconds: 60,
                }),
                node_policy: NodeHistoryPolicyProvenance::SignedConfig {
                    path: PathBuf::from(ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG),
                    space: ItemSpace::Project,
                    content_hash: "44".repeat(32),
                    signer_fingerprint: "55".repeat(32),
                },
            },
        };

        let captured = capture_thread_history_policy(&policy).unwrap();
        assert_eq!(
            captured.item_trust_class,
            ryeos_state::objects::CapturedItemTrustClass::Trusted
        );
        assert!(matches!(
            &captured.resolved_from,
            ryeos_state::objects::CapturedPolicyProvenance::ItemAuthored {
                effective_trust_class:
                    ryeos_state::objects::CapturedEffectiveTrustClass::TrustedProject,
                minimum_clamp: Some(ryeos_state::objects::CapturedThreadHistoryMinimumClamp {
                    requested_seconds: 30,
                    minimum_seconds: 60,
                }),
                ..
            }
        ));
        let wire = serde_json::to_value(captured).unwrap();
        assert_eq!(wire["item_trust_class"], json!("trusted"));
        assert_eq!(
            wire["resolved_from"]["item_authored"]["effective_trust_class"],
            json!("trusted_project")
        );
        assert_eq!(
            wire["resolved_from"]["item_authored"]["node_policy"]["signed_config"]["space"],
            json!("project")
        );
        assert_eq!(
            wire["resolved_from"]["item_authored"]["node_policy"]["signed_config"]["path"],
            json!(ryeos_engine::history_policy::NODE_HISTORY_POLICY_CONFIG)
        );
    }

    #[test]
    fn managed_envelope_is_native_success_with_outputs_and_cost() {
        let result = json!("directive_return");
        let outputs = json!({ "answer": 42 });
        let raw_cost = json!({ "input_tokens": 120, "output_tokens": 45, "total_usd": 0.0012 });
        let env = managed_runtime_envelope(
            "completed",
            Some(&result),
            None,
            Some(&raw_cost),
            &outputs,
            &["w1".to_string()],
        );
        assert_eq!(env["success"], json!(true));
        assert_eq!(env["status"], json!("completed"));
        assert_eq!(env["result"], result);
        // Structured outputs + warnings are preserved verbatim (raw cost too).
        assert_eq!(env["outputs"], outputs);
        assert_eq!(env["warnings"], json!(["w1"]));
        assert_eq!(env["cost"]["input_tokens"], json!(120));
    }

    #[test]
    fn degraded_envelope_is_visible_failure() {
        let child_result = json!({ "answer": 42 });
        let env = degraded_follow_envelope("completed", Some(&child_result), None, None);
        // A lost canonical envelope becomes a visible in-band FAILURE so the parent
        // resumes into on_error, not a silent empty success.
        assert_eq!(env["success"], json!(false));
        assert_eq!(env["status"], json!("failed"));
        assert_eq!(env["result"]["code"], json!(DEGRADED_FOLLOW_ENVELOPE_CODE));
        assert_eq!(env["result"]["child_status"], json!("completed"));
        assert_eq!(env["result"]["child_result"], child_result);
        assert_eq!(env["cost"], json!(null));
    }

    fn waiter(phase: &str, terminal: Option<&str>) -> crate::runtime_db::FollowWaiter {
        crate::runtime_db::FollowWaiter {
            follow_key: "parent-1/gr-1/n_follow/3".to_string(),
            parent_thread_id: "parent-1".to_string(),
            parent_chain_root_id: "chain-parent".to_string(),
            parent_successor_thread_id: Some("succ-1".to_string()),
            follow_node: "n_follow".to_string(),
            graph_run_id: "gr-1".to_string(),
            step_count: 3,
            frontier_id: None,
            fanout: false,
            expected_children: 1,
            children: vec![crate::runtime_db::FollowWaiterChild {
                item_index: 0,
                item_ref: "directive:example/child".to_string(),
                spec_hash: "spec-1".to_string(),
                child_thread_id: "child-1".to_string(),
                child_chain_root_id: "chain-child".to_string(),
                terminal_thread_id: terminal.map(|_| "child-tail".to_string()),
                terminal_status: terminal.map(str::to_string),
                terminal_envelope: None,
                created_at_ms: 0,
                updated_at_ms: 0,
            }],
            phase: phase.to_string(),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn follow_fact_suspended_parent_wire_shape() {
        let f =
            FollowFact::suspended_parent(&waiter(crate::runtime_db::follow_phase::WAITING, None));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["role"], json!("suspended_parent"));
        assert_eq!(v["display_state"], json!("suspended"));
        assert_eq!(v["phase"], json!("waiting"));
        assert_eq!(v["follow_node"], json!("n_follow"));
        assert_eq!(v["child_thread_id"], json!("child-1"));
        assert_eq!(v["child_chain_root_id"], json!("chain-child"));
        // Still running → terminal status present but null.
        assert!(v.get("child_terminal_status").is_some());
        assert_eq!(v["child_terminal_status"], json!(null));
        assert_eq!(v["parent_successor_thread_id"], json!("succ-1"));
    }

    #[test]
    fn follow_fact_resume_successor_live_omits_phase_carries_terminal() {
        let f = FollowFact::resume_successor_live(&waiter(
            crate::runtime_db::follow_phase::READY,
            Some("completed"),
        ));
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["role"], json!("resume_successor"));
        assert_eq!(v["display_state"], json!("resumed"));
        // `phase` is a suspended_parent-only field.
        assert!(
            v.get("phase").is_none(),
            "resume_successor carries no phase"
        );
        assert_eq!(v["child_terminal_status"], json!("completed"));
        assert_eq!(v["child_chain_root_id"], json!("chain-child"));
    }

    #[test]
    fn follow_fact_resume_successor_durable_is_minimal() {
        let f = FollowFact::resume_successor_durable("succ-9");
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["role"], json!("resume_successor"));
        assert_eq!(v["display_state"], json!("resumed"));
        assert!(v.get("phase").is_none());
        assert!(v.get("follow_node").is_none());
        assert!(v.get("child_thread_id").is_none());
        assert!(v.get("child_chain_root_id").is_none());
        // Present-but-null (the field is always emitted when the fact exists).
        assert_eq!(v["child_terminal_status"], json!(null));
        assert_eq!(v["parent_successor_thread_id"], json!("succ-9"));
    }

    #[test]
    fn thread_view_omits_absent_follow_and_zero_pending() {
        // A non-follow, un-steered thread pays nothing for the new fields.
        let view = ThreadListView {
            item: ThreadListItem {
                thread_id: "T-x".to_string(),
                chain_root_id: "T-x".to_string(),
                kind: "directive".to_string(),
                status: "running".to_string(),
                item_ref: "directive:demo".to_string(),
                launch_mode: "managed_async".to_string(),
                current_site_id: "site:test".to_string(),
                origin_site_id: "site:test".to_string(),
                upstream_thread_id: None,
                successor_thread_id: None,
                requested_by: None,
                project_root: None,
                created_at: "t".to_string(),
                updated_at: "t".to_string(),
            },
            execution: ExecutionFacts {
                supports_continuation: true,
                supports_operator_followup: true,
            },
            follow: None,
            project: None,
            pending: 0,
            facets: std::collections::BTreeMap::new(),
            current_node: None,
        };
        let v = serde_json::to_value(&view).unwrap();
        assert!(v.get("follow").is_none(), "absent follow omitted");
        assert!(v.get("pending").is_none(), "zero pending omitted");
        // Execution facts still flatten as before.
        assert_eq!(v["execution"]["supports_continuation"], json!(true));

        // A staged input surfaces the count.
        let steered = ThreadListView { pending: 2, ..view };
        let v2 = serde_json::to_value(&steered).unwrap();
        assert_eq!(v2["pending"], json!(2));
    }
}
