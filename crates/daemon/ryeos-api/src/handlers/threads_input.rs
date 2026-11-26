//! `threads.input` — deliver operator input to a directive thread chain.
//!
//! The seat's ground verb lands here. The service owns the delivery
//! decision; clients never know there are two mechanisms:
//!
//! - a `fresh` target supplies a complete execution identity and starts an
//!   independent chain;
//! - a settled completed-or-failed `thread` target inherits the predecessor's
//!   persisted execution identity and creates a successor;
//! - running directive thread → fold the operator input into the in-flight
//!   loop as a new `cognition_in` (steer at the next turn boundary, or after a
//!   forced interrupt) via the live-input queue; the runtime drains and
//!   persists it through `runtime.poll_input`;
//! - otherwise (not-yet-running, or a settled non-continuable status) →
//!   structured refusal.
//!
//! A `thread` target may also carry `rebind`: ref bindings the operator wants
//! the continuation to run with. It applies only when a successor is created.
//! A binding that names what the thread already runs with changes nothing and
//! takes the ordinary continuation. Input folded into a running thread keeps
//! that thread's bindings; the result says the rebind was not applied, and
//! nothing is held for later.
//!
//! Submit result contract (input-design.md):
//! `{ thread_id?, delivery: launched|submitted|refused, notice? }`.
//! A live fold returns `submitted` with the SAME `thread_id` (no new thread).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::registry::ServiceDescriptor;
use ryeos_app::live_input_queue::{
    EnqueueOutcome, MAX_LIVE_INPUT_SERIALIZED_BYTES, serialized_live_input_bytes,
};
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{LiveInput, LiveInputIntent, ThreadStatus};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Operator text, bound whole — never tokenized here.
    pub input: String,
    /// Structurally separates a fresh execution identity from a thread whose
    /// persisted identity is authoritative.
    pub target: Target,
    /// Delivery intent for a *running* directive thread. Ignored on the
    /// launch / chained-resume paths (those land at turn boundaries by
    /// construction). Omission means cooperative `Steer`.
    #[serde(default)]
    pub intent: LiveInputIntent,
    /// Optional title for the thread this input launches, stamped as the
    /// `title` facet in the same commit as the thread. Absent: a fresh launch
    /// derives one from `input`, a continuation inherits its predecessor's.
    /// Unused when the input folds into a running thread.
    #[serde(default)]
    pub title: Option<String>,
}

use ryeos_app::thread_lifecycle::TITLE_FACET_KEY;
/// Longest accepted title, in Unicode scalar values.
const TITLE_MAX_SCALARS: usize = 160;
/// Longest stretch of `input` a derived title keeps, in Unicode scalar values.
const DERIVED_TITLE_MAX_SCALARS: usize = 80;

/// An explicit title: trimmed, 1..=160 Unicode scalars, no control characters.
fn validate_title(raw: &str) -> Result<String, HandlerError> {
    let title = raw.trim();
    let scalars = title.chars().count();
    if scalars == 0 || scalars > TITLE_MAX_SCALARS {
        return Err(HandlerError::BadRequest(format!(
            "title must be 1..={TITLE_MAX_SCALARS} characters after trimming; got {scalars}"
        )));
    }
    if title.chars().any(char::is_control) {
        return Err(HandlerError::BadRequest(
            "title must not contain control characters".to_string(),
        ));
    }
    Ok(title.to_string())
}

/// The default title for operator `input`: its first non-empty line with
/// whitespace and control characters collapsed to single spaces, cut to at
/// most 80 Unicode scalar values on a scalar boundary, with a single `…`
/// appended only when something was cut. `None` when no line has any text.
fn derive_title(input: &str) -> Option<String> {
    let line = input.split(['\n', '\r']).find_map(|line| {
        let collapsed = line
            .split(|c: char| c.is_whitespace() || c.is_control())
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        (!collapsed.is_empty()).then_some(collapsed)
    })?;
    let mut scalars = line.chars();
    let kept: String = scalars.by_ref().take(DERIVED_TITLE_MAX_SCALARS).collect();
    if scalars.next().is_none() {
        return Some(kept);
    }
    Some(format!("{}…", kept.trim_end()))
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Target {
    Fresh {
        item_ref: String,
        project_path: String,
        ref_bindings: BTreeMap<String, String>,
    },
    Thread {
        thread_id: String,
        /// Ref bindings to rebind on the continuation, name → canonical ref.
        /// Each is authorised and resolved like a binding of a fresh launch,
        /// and admitted only where the thread's runtime contract lets an
        /// operator rebind that name.
        #[serde(default)]
        rebind: BTreeMap<String, String>,
    },
}

fn is_settled(status: &str) -> bool {
    matches!(
        ThreadStatus::from_str_lossy(status),
        Some(
            ThreadStatus::Completed
                | ThreadStatus::Failed
                | ThreadStatus::Cancelled
                | ThreadStatus::Killed
                | ThreadStatus::TimedOut
                | ThreadStatus::Continued
        )
    )
}

/// Statuses a follow-up can braid a successor onto — must match the terminal
/// sources `StateStore::create_continuation` accepts. A settled turn outside
/// this set (`continued`/`cancelled`/`killed`/`timed_out`) is refused
/// synchronously rather than launched, so the caller never watches a pre-minted
/// successor id that was never persisted.
fn is_continuable(status: &str) -> bool {
    matches!(
        ThreadStatus::from_str_lossy(status),
        Some(ThreadStatus::Completed | ThreadStatus::Failed)
    )
}

/// The operator-facing notice for an accepted live steer, reporting the current
/// staged depth (the input queued behind any not-yet-folded ones). `pending` is
/// the queue depth AFTER this input was enqueued, so it is always ≥ 1.
fn staged_notice(pending: usize) -> String {
    format!("Input queued ({pending} staged).")
}

/// Where a follow-up to an existing thread lands, decided purely from status.
#[derive(Debug, PartialEq, Eq)]
enum FollowUpDecision {
    /// Running thread → fold the input into its in-flight loop (live).
    Live,
    /// Settled completed/failed turn → braid a successor (chained-resume).
    Continue,
    /// Not deliverable; carries the operator-facing notice.
    Refuse(String),
}

/// Classify a follow-up by the predecessor's STATUS only (kind eligibility is
/// decided separately, since it needs daemon policy). Pure, so it is
/// unit-testable without a daemon and runs synchronously.
///
/// This decides delivery TARGET, not idempotency: it does NOT refuse merely
/// because a successor already exists — an idempotent double-submit is deduped
/// downstream by fingerprint in `create_or_get_operator_continuation` (same
/// input → same successor; different input → conflict).
fn classify_follow_up(status: &str, thread_id: &str) -> FollowUpDecision {
    if ThreadStatus::from_str_lossy(status) == Some(ThreadStatus::Running) {
        // A running thread folds operator input as a new cognition_in — steered
        // at the next turn boundary, or after a forced interrupt.
        return FollowUpDecision::Live;
    }
    if !is_settled(status) {
        // created / accepted / pending: no live loop to steer yet, and no
        // settled turn to braid onto. Refuse honestly; the operator retries
        // once it is running or has finished.
        return FollowUpDecision::Refuse(format!(
            "thread {thread_id} is {status}; operator input needs a running thread \
             or a completed/failed turn to continue"
        ));
    }
    if !is_continuable(status) {
        return FollowUpDecision::Refuse(format!(
            "thread {thread_id} is {status}; a follow-up can only continue a completed or failed turn"
        ));
    }
    FollowUpDecision::Continue
}

fn ensure_execution_identity_continuity(
    previous: &ryeos_app::state_store::ThreadDetail,
    prior_resume: &ryeos_app::launch_metadata::ResumeContext,
) -> Result<(), HandlerError> {
    let mut mismatches = Vec::new();
    if prior_resume.kind != previous.kind {
        mismatches.push("kind");
    }
    if prior_resume.item_ref != previous.item_ref {
        mismatches.push("item_ref");
    }
    if prior_resume.executor_ref.as_deref() != Some(previous.executor_ref.as_str()) {
        mismatches.push("executor_ref");
    }
    if prior_resume.launch_mode != previous.launch_mode {
        mismatches.push("launch_mode");
    }
    if prior_resume.current_site_id != previous.current_site_id {
        mismatches.push("current_site_id");
    }
    if prior_resume.origin_site_id != previous.origin_site_id {
        mismatches.push("origin_site_id");
    }
    if previous.requested_by.as_deref() != Some(prior_resume.principal_identifier()) {
        mismatches.push("requested_by");
    }
    if previous.lifecycle_authority != Some(prior_resume.lifecycle_authority) {
        mismatches.push("lifecycle_authority");
    }

    let row_authority = previous.project_authority.as_ref();
    if row_authority != Some(&prior_resume.project_authority) {
        mismatches.push("project_authority");
    }
    let row_root_projection = previous.project_root.as_deref().map(std::path::Path::new);
    if row_root_projection != prior_resume.project_authority.project_root_projection()
        || row_authority.and_then(|authority| authority.project_root_projection())
            != row_root_projection
    {
        mismatches.push("project_root_projection");
    }
    if row_authority.and_then(|authority| authority.operational_snapshot_projection())
        != prior_resume
            .project_authority
            .operational_snapshot_projection()
    {
        mismatches.push("base_project_snapshot_projection");
    }

    if mismatches.is_empty() {
        return Ok(());
    }
    Err(HandlerError::Structured {
        code: "execution_identity_continuity_mismatch".to_string(),
        status: axum::http::StatusCode::CONFLICT.as_u16(),
        body: json!({
            "code": "execution_identity_continuity_mismatch",
            "error": "predecessor row and persisted execution identity disagree",
            "thread_id": previous.thread_id,
            "mismatches": mismatches,
        }),
    })
}

fn authorize_execution_refs(
    item_ref: &str,
    ref_bindings: &BTreeMap<String, String>,
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<(), HandlerError> {
    authorize_execution_refs_for(item_ref, ref_bindings, &ctx.scopes, &state.authorizer)
}

fn authorize_execution_refs_for(
    item_ref: &str,
    ref_bindings: &BTreeMap<String, String>,
    scopes: &[String],
    authorizer: &ryeos_runtime::authorizer::Authorizer,
) -> Result<(), HandlerError> {
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(ref_bindings)
        .map_err(dispatch_error)?;
    for (label, value) in std::iter::once(("item_ref", item_ref)).chain(
        ref_bindings
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    ) {
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
            .map_err(|error| HandlerError::BadRequest(format!("invalid {label}: {error}")))?;
        let required = ryeos_runtime::authorizer::canonical_cap(
            &canonical.kind,
            &canonical.bare_id,
            "execute",
        );
        let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(&required);
        authorizer.authorize(scopes, &policy).map_err(|_| {
            HandlerError::Forbidden(format!("missing required capability: {required}"))
        })?;
    }
    Ok(())
}

/// What became of the rebind a request carried: `None` when it carried none.
///
/// `applied` is true only when the returned successor runs with the requested
/// bindings. Otherwise `reason` is `unchanged` (they already were the thread's
/// bindings), `thread_running` (the input was folded into the live thread,
/// which keeps its bindings) or `conflict` (the thread already continued
/// another way).
fn rebind_report(
    requested: &BTreeMap<String, String>,
    outcome: RebindOutcome<'_>,
) -> Option<Value> {
    if requested.is_empty() {
        return None;
    }
    Some(match outcome {
        RebindOutcome::Applied(changes) if !changes.is_empty() => json!({
            "applied": true,
            "names": changes.keys().collect::<Vec<_>>(),
        }),
        RebindOutcome::Applied(_) => json!({"applied": false, "reason": "unchanged"}),
        RebindOutcome::ThreadRunning => json!({"applied": false, "reason": "thread_running"}),
        RebindOutcome::Conflict => json!({"applied": false, "reason": "conflict"}),
    })
}

enum RebindOutcome<'a> {
    /// A successor runs with these changed bindings (none when the request
    /// named only what the thread already ran with).
    Applied(&'a BTreeMap<String, String>),
    ThreadRunning,
    Conflict,
}

/// Syntax of a requested rebind: daemon-wide name and size limits, and a
/// canonical ref at every name. Says nothing about permission.
fn validate_rebind_request(rebind: &BTreeMap<String, String>) -> Result<(), HandlerError> {
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(rebind)
        .map_err(dispatch_error)?;
    for (name, value) in rebind {
        ryeos_engine::canonical_ref::CanonicalRef::parse(value).map_err(|error| {
            HandlerError::BadRequest(format!("invalid rebind `{name}`: {error}"))
        })?;
    }
    Ok(())
}

/// Whether an existing successor was born from this same operator request:
/// the same input, the same sender, and the same ref bindings.
///
/// The comparison reads the bindings recorded in the successor's launch
/// identity at its birth. Nothing is resolved again, so a retry of a rebind
/// that already produced a successor finds it even after the bound item or
/// the signed configuration changed.
fn born_from_same_request(
    successor: &ryeos_app::launch_metadata::ResumeContext,
    requested: &ryeos_app::launch_metadata::ResumeContext,
) -> bool {
    successor.ref_bindings == requested.ref_bindings
        && successor.parameters == requested.parameters
        && successor.requested_by == requested.requested_by
}

/// Launch an operator successor that exists but was left `created`: prepare
/// it from its own persisted launch metadata and require the same handoff
/// proof as a newly created successor. Nothing about its birth is recomputed.
async fn redrive_created_operator_successor(
    state: &Arc<AppState>,
    successor_id: &str,
) -> Result<(), HandlerError> {
    let launch_metadata = state
        .state_store
        .get_launch_metadata(successor_id)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
        .ok_or_else(|| {
            HandlerError::Internal(format!(
                "operator successor {successor_id} has no persisted launch metadata"
            ))
        })?;
    let prepared = ryeos_executor::execution::launch::prepare_existing_operator_successor_launch(
        state,
        successor_id,
        &launch_metadata,
    )
    .await
    .map_err(build_and_launch_error)?;
    let task_id = successor_id.to_string();
    let st = (**state).clone();
    let (handoff, ready) = ryeos_executor::execution::launch::LaunchHandoff::channel();
    let task = tokio::spawn(async move {
        ryeos_executor::execution::launch::launch_prepared_operator_successor(
            st, &task_id, prepared, &handoff,
        )
        .await
    });
    let ready_thread_id = await_operator_handoff(task, ready).await?;
    if ready_thread_id != successor_id {
        return Err(HandlerError::Internal(
            "operator relaunch handoff returned a different thread identity".to_string(),
        ));
    }
    Ok(())
}

/// Attach the rebind report to a result that carried a rebind request.
fn with_rebind_report(mut result: Value, report: Option<Value>) -> Value {
    if let (Some(report), Some(object)) = (report, result.as_object_mut()) {
        object.insert("rebind".to_string(), report);
    }
    result
}

/// The durable record of an operator rebind, committed in the successor's
/// birth batch: which thread was continued, each replaced ref binding, and
/// the launch's runtime facts before and after. Only the daemon writes it.
fn launch_binding_rebound_event(
    rebound: &ryeos_executor::execution::launch_rebind::ReboundLaunch,
) -> ryeos_app::state_store::NewEventRecord {
    let event_type = ryeos_runtime::RuntimeEventType::LaunchBindingRebound;
    ryeos_app::state_store::NewEventRecord {
        event_type: event_type.as_str().to_owned(),
        storage_class: event_type.storage_class().as_str().to_owned(),
        payload: json!({
            "source_thread_id": rebound.source_thread_id,
            "changes": rebound
                .changes
                .iter()
                .map(|change| json!({
                    "input": "ref_binding",
                    "name": change.name,
                    "from": change.from,
                    "to": change.to,
                }))
                .collect::<Vec<_>>(),
            "from_facts": rebound.from_facts,
            "to_facts": rebound.to_facts,
        }),
    }
}

fn dispatch_error(error: ryeos_executor::dispatch_error::DispatchError) -> HandlerError {
    HandlerError::Structured {
        code: error.code().to_string(),
        status: error.http_status().as_u16(),
        body: ryeos_executor::structured_error::dispatch_error_value(&error),
    }
}

fn launch_spawn_error(error: crate::routes::launch::LaunchSpawnError) -> HandlerError {
    match error {
        crate::routes::launch::LaunchSpawnError::Dispatch(error) => dispatch_error(error),
        crate::routes::launch::LaunchSpawnError::InvalidRef { ref_str, reason } => {
            HandlerError::BadRequest(format!("invalid item_ref '{ref_str}': {reason}"))
        }
        error => HandlerError::Structured {
            code: error.code().to_string(),
            status: error.http_status().as_u16(),
            body: json!({
                "code": error.code(),
                "error": error.to_string(),
            }),
        },
    }
}

fn build_and_launch_error(
    error: ryeos_executor::execution::launch::BuildAndLaunchError,
) -> HandlerError {
    use ryeos_executor::execution::launch::BuildAndLaunchError;
    match error {
        BuildAndLaunchError::MissingSecrets { item_ref, secrets } => {
            let Some(missing) = secrets.first() else {
                return HandlerError::Internal(
                    "missing-secret launch error carried no secret".to_string(),
                );
            };
            let source = missing.primary_source();
            dispatch_error(
                ryeos_executor::dispatch_error::DispatchError::RequiredSecretMissing {
                    item_ref,
                    env_var: missing.name.clone(),
                    source_kind: source.kind_for_wire().to_string(),
                    source_name: source.name_for_wire(),
                    remediation: ryeos_executor::dispatch_error::required_secret_remediation(
                        &missing.name,
                    ),
                },
            )
        }
        BuildAndLaunchError::CapabilityRejected { reason } => dispatch_error(
            ryeos_executor::dispatch_error::DispatchError::CapabilityRejected { reason },
        ),
        BuildAndLaunchError::LaunchPreparation(error) => dispatch_error(*error),
        BuildAndLaunchError::LaunchCancelled { stage, .. } => {
            dispatch_error(ryeos_executor::dispatch_error::DispatchError::LaunchCancelled { stage })
        }
        BuildAndLaunchError::Materialization(error) => {
            HandlerError::Internal(format!("launch materialization failed: {error}"))
        }
        error @ BuildAndLaunchError::RuntimeCleanupUnresolved { .. } => {
            HandlerError::Internal(error.to_string())
        }
        BuildAndLaunchError::Internal(error) => HandlerError::Internal(error.to_string()),
    }
}

fn handoff_failure(
    failure: ryeos_executor::execution::launch::LaunchHandoffFailure,
) -> HandlerError {
    HandlerError::Structured {
        code: failure.code,
        status: failure.status,
        body: failure.body,
    }
}

async fn await_dispatch_handoff(
    mut task: tokio::task::JoinHandle<Result<(), crate::routes::launch::LaunchSpawnError>>,
    ready: tokio::sync::oneshot::Receiver<ryeos_executor::execution::launch::LaunchHandoffResult>,
) -> Result<String, HandlerError> {
    tokio::select! {
        readiness = ready => match readiness {
            Ok(Ok(thread_id)) => Ok(thread_id),
            Ok(Err(failure)) => Err(handoff_failure(failure)),
            Err(_) => match task.await {
                Ok(Err(error)) => Err(launch_spawn_error(error)),
                Ok(Ok(())) => Err(HandlerError::Internal(
                    "launch completed without authoritative handoff".to_string(),
                )),
                Err(error) => Err(HandlerError::Internal(format!("launch task failed: {error}"))),
            },
        },
        result = &mut task => match result {
            Ok(Err(error)) => Err(launch_spawn_error(error)),
            Ok(Ok(())) => Err(HandlerError::Internal(
                "launch completed without authoritative handoff".to_string(),
            )),
            Err(error) => Err(HandlerError::Internal(format!("launch task failed: {error}"))),
        },
    }
}

async fn await_operator_handoff(
    mut task: tokio::task::JoinHandle<
        Result<
            ryeos_executor::execution::launch::SuccessorLaunchOutcome,
            ryeos_executor::execution::launch::BuildAndLaunchError,
        >,
    >,
    ready: tokio::sync::oneshot::Receiver<ryeos_executor::execution::launch::LaunchHandoffResult>,
) -> Result<String, HandlerError> {
    tokio::select! {
        readiness = ready => match readiness {
            Ok(Ok(thread_id)) => Ok(thread_id),
            Ok(Err(failure)) => Err(handoff_failure(failure)),
            Err(_) => match task.await {
                Ok(Err(error)) => Err(build_and_launch_error(error)),
                Ok(Ok(_)) => Err(HandlerError::Internal(
                    "operator successor completed without authoritative handoff".to_string(),
                )),
                Err(error) => Err(HandlerError::Internal(format!(
                    "operator successor task failed: {error}"
                ))),
            },
        },
        result = &mut task => match result {
            Ok(Err(error)) => Err(build_and_launch_error(error)),
            Ok(Ok(_)) => Err(HandlerError::Internal(
                "operator successor completed without authoritative handoff".to_string(),
            )),
            Err(error) => Err(HandlerError::Internal(format!(
                "operator successor task failed: {error}"
            ))),
        },
    }
}

struct FreshLaunchAdmission<'a> {
    item_ref: &'a crate::routes::parsed_ref::ParsedItemRef,
    ref_bindings: &'a BTreeMap<String, String>,
    project: &'a ryeos_executor::execution::project_source::ResolvedProjectContext,
    provenance: &'a ryeos_app::execution_provenance::ExecutionProvenance,
    parameters: &'a Value,
    ctx: &'a HandlerContext,
    state: &'a AppState,
    lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
}

fn admit_fresh_launch(
    admission: FreshLaunchAdmission<'_>,
) -> Result<crate::routes::launch::DispatchLaunchOptions, HandlerError> {
    let FreshLaunchAdmission {
        item_ref,
        ref_bindings,
        project,
        provenance,
        parameters,
        ctx,
        state,
        lifecycle_authority,
    } = admission;
    let preflight = crate::routes::launch::preflight_dispatch_launch(
        state,
        item_ref,
        project,
        provenance,
        parameters,
        ref_bindings,
        &Vec::new(),
        &ctx.fingerprint,
        &ctx.scopes,
        state.threads.site_id(),
        None,
        "wait",
        false,
        None,
        None,
        None,
    )
    .map_err(dispatch_error)?;
    if !preflight.class.persists_pre_minted_root() {
        return Err(HandlerError::BadRequest(
            "fresh threads.input target requires a launch-producing item".to_string(),
        ));
    }
    let root_admission = preflight.root_admission.ok_or_else(|| {
        HandlerError::Internal("threaded dispatch preflight returned no root admission".to_string())
    })?;
    crate::routes::launch::DispatchLaunchOptions::admitted(
        root_admission,
        preflight.root_dispatch_evidence,
        &project.effective_path,
        ref_bindings.clone(),
        Vec::new(),
        lifecycle_authority,
        Some(ctx.clone()),
    )
    .map_err(|error| {
        HandlerError::Internal(format!(
            "validated fresh-launch contract rejected at dispatch boundary: {error:#}"
        ))
    })
}

pub async fn handle(
    req: Request,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value, HandlerError> {
    ctx.require_verified()?;
    let Request {
        input,
        target,
        intent,
        title,
    } = req;
    if input.trim().is_empty() {
        return Err(HandlerError::BadRequest("input is empty".to_string()));
    }
    let explicit_title = title.as_deref().map(validate_title).transpose()?;
    let is_interrupt = intent.is_interrupt();
    let operator_input = LiveInput::new(input.clone(), intent);
    let input_bytes = serialized_live_input_bytes(&operator_input);
    if input_bytes > MAX_LIVE_INPUT_SERIALIZED_BYTES {
        return Err(HandlerError::BadRequest(format!(
            "operator input is {input_bytes} serialized bytes; maximum is {MAX_LIVE_INPUT_SERIALIZED_BYTES}"
        )));
    }

    // Full daemon-authored execution facts for a kind — every arm returns both
    // so the client gates the operator-input affordance on
    // `supports_operator_followup`, not on `supports_continuation` alone.
    let exec_facts = |kind: &str| {
        json!({
            "supports_continuation": state.threads.supports_continuation_for_kind(kind),
            "supports_operator_followup": state.threads.supports_operator_followup_for_kind(kind),
        })
    };

    let (thread_id, rebind) = match target {
        Target::Fresh {
            item_ref,
            project_path,
            ref_bindings,
        } => {
            authorize_execution_refs(&item_ref, &ref_bindings, &ctx, &state)?;
            let project_path =
                crate::routes::abs_path::AbsolutePathBuf::try_new(project_path.into())
                    .map_err(|error| HandlerError::BadRequest(format!("project_path: {error}")))?;
            let parameters = json!({ "input": input });
            let parsed_ref = crate::routes::parsed_ref::ParsedItemRef::parse(&item_ref)
                .map_err(|error| HandlerError::BadRequest(format!("item_ref: {error}")))?;
            let thread_id = ryeos_app::thread_lifecycle::new_thread_id();
            ryeos_app::execution_policy::authorize_standard_local_live_execution(&ctx.scopes)
                .map_err(|error| HandlerError::Forbidden(format!("{error:#}")))?;
            let mut project_ctx =
                ryeos_executor::execution::project_source::resolve_project_context(
                    &state,
                    &ryeos_executor::execution::project_source::ProjectSource::LiveFs,
                    project_path.as_path(),
                    &ctx.fingerprint,
                    &format!("thread-input-{thread_id}"),
                    None,
                )
                .map_err(|error| {
                    HandlerError::BadRequest(format!("capture fresh thread project: {error}"))
                })?;
            let resolved_authority =
                ryeos_app::execution_policy::resolve_standard_local_live_authority(
                    &project_ctx.effective_path,
                    ctx.scopes.clone(),
                    &ctx.fingerprint,
                    &state.isolation,
                )
                .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
            let launch_provenance =
                ryeos_app::execution_provenance::ExecutionProvenance::root_live_fs(
                    project_ctx.effective_path.clone(),
                    project_ctx.request_engine.clone(),
                    resolved_authority.project.clone(),
                )
                .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
            let launch_options = admit_fresh_launch(FreshLaunchAdmission {
                item_ref: &parsed_ref,
                ref_bindings: &ref_bindings,
                project: &project_ctx,
                provenance: &launch_provenance,
                parameters: &parameters,
                ctx: &ctx,
                state: &state,
                lifecycle_authority: resolved_authority.lifecycle,
            })?
            .retain_captured_generation(project_ctx.take_captured_generation())
            .with_initial_facets(
                explicit_title
                    .clone()
                    .or_else(|| derive_title(&input))
                    .map(|title| BTreeMap::from([(TITLE_FACET_KEY.to_string(), title)]))
                    .unwrap_or_default(),
            );
            let (handle, ready) = crate::routes::launch::spawn_dispatch_launch_with_handoff(
                &state,
                parsed_ref,
                parameters,
                ctx.fingerprint.clone(),
                ctx.scopes.clone(),
                thread_id.clone(),
                launch_provenance,
                launch_options,
            );
            let ready_thread_id = await_dispatch_handoff(handle, ready).await?;
            if ready_thread_id != thread_id {
                return Err(HandlerError::Internal(
                    "launch handoff returned a different thread identity".to_string(),
                ));
            }
            return Ok(json!({
                "thread_id": thread_id,
                "delivery": "launched",
                "notice": Value::Null,
            }));
        }
        Target::Thread { thread_id, rebind } => {
            // Checked before the thread is even looked up, so a malformed
            // rebind is a bad request on every path, including input folded
            // into a running thread, where the rebind is otherwise unused.
            validate_rebind_request(&rebind)?;
            (thread_id, rebind)
        }
    };

    // Resolve the prior turn. Eligibility only — a
    // live source or a non-continuable settled status is refused; an existing
    // successor is NOT a refusal (the create-or-get below dedups by fingerprint).
    let previous = {
        let detail = state
            .state_store
            .get_thread(&thread_id)
            .map_err(|e| HandlerError::Internal(format!("{e:#}")))?
            .ok_or(HandlerError::NotFound)?;
        ctx.require_owner(detail.requested_by.as_deref())?;

        // Kind eligibility, enforced at the API boundary, applies to BOTH
        // live delivery and settled continuation: a kind that cannot
        // chain-fold (e.g. graph), or that self-continues by machine only,
        // has nowhere to put operator input. Refuse cleanly with
        // authoritative execution facts rather than letting the lifecycle
        // guard surface a 500. (Decided before status so a stale/third-party
        // client that skipped the client-side gate gets a structured
        // refusal regardless of whether the thread is live or settled.)
        if !state.threads.supports_continuation_for_kind(&detail.kind) {
            return Ok(json!({
                "thread_id": Value::Null,
                "delivery": "refused",
                "notice": format!(
                    "thread {} is kind '{}', which does not support continuation",
                    detail.thread_id, detail.kind
                ),
                "execution": exec_facts(&detail.kind),
            }));
        }
        if !state
            .threads
            .supports_operator_followup_for_kind(&detail.kind)
        {
            return Ok(json!({
                "thread_id": Value::Null,
                "delivery": "refused",
                "notice": format!(
                    "thread {} is kind '{}', which self-continues by machine only \
                     and does not accept operator follow-up",
                    detail.thread_id, detail.kind
                ),
                "execution": exec_facts(&detail.kind),
            }));
        }

        match classify_follow_up(&detail.status, &detail.thread_id) {
            // Running, eligible directive: fold operator input into the
            // in-flight loop. Enqueue first (so the input is present when
            // the runtime next polls), then — for an interrupt — nudge the
            // runtime to cut its current cognition.
            FollowUpDecision::Live => {
                let pending = match state
                    .live_input
                    .enqueue(&detail.thread_id, operator_input.clone())
                {
                    EnqueueOutcome::Accepted { pending } => pending,
                    EnqueueOutcome::Full { pending } => {
                        return Ok(json!({
                            "thread_id": Value::Null,
                            "delivery": "refused",
                            "notice": format!(
                                "thread {} already has {} operator input(s) pending; \
                                 wait for them to fold",
                                detail.thread_id, pending
                            ),
                            "execution": exec_facts(&detail.kind),
                        }));
                    }
                    EnqueueOutcome::Closed => {
                        // Finalized between the status read and the enqueue.
                        // The live window closed; resubmitting takes the
                        // settled continuation path.
                        return Ok(json!({
                            "thread_id": Value::Null,
                            "delivery": "refused",
                            "notice": format!(
                                "thread {} finalized before the input could be delivered; \
                                 resubmit to continue",
                                detail.thread_id
                            ),
                            "execution": exec_facts(&detail.kind),
                        }));
                    }
                    EnqueueOutcome::TooLarge { bytes, max } => {
                        return Err(HandlerError::BadRequest(format!(
                            "operator input is {bytes} serialized bytes; maximum is {max}"
                        )));
                    }
                };

                // Default notice reports the staged depth so the operator can
                // see input piling up before the runtime folds it. An
                // interrupt that failed to signal overrides it with the
                // degrade message below.
                let mut notice = json!(staged_notice(pending));
                // Interrupt: cut the in-flight cognition. The input is
                // already enqueued, so on any signal failure we degrade to a
                // cooperative steer (it still folds at the next boundary).
                if is_interrupt {
                    let outcome = match detail.runtime.process_identity.as_ref() {
                        Some(identity) => ryeos_app::process::interrupt_process(identity),
                        None if detail.runtime.pgid.is_some() => {
                            ryeos_app::process::SignalResult::MissingIdentity
                        }
                        None => ryeos_app::process::SignalResult::MissingPgid,
                    };
                    if outcome != ryeos_app::process::SignalResult::Delivered {
                        notice = json!(format!(
                            "interrupt not delivered ({}); steering instead — \
                                 input folds at the next turn boundary",
                            outcome.as_str()
                        ));
                        tracing::warn!(
                            thread_id = %detail.thread_id,
                            signal = %outcome.as_str(),
                            "live interrupt signal not delivered; degraded to steer"
                        );
                    }
                }

                // The live thread keeps the bindings it was admitted with. A
                // rebind carried with folded input is reported as not applied
                // and is not held for a later continuation.
                return Ok(with_rebind_report(
                    json!({
                        "thread_id": detail.thread_id,
                        "delivery": "submitted",
                        "notice": notice,
                        "pending": pending,
                        "execution": exec_facts(&detail.kind),
                    }),
                    rebind_report(&rebind, RebindOutcome::ThreadRunning),
                ));
            }
            FollowUpDecision::Refuse(notice) => {
                return Ok(json!({
                    "thread_id": Value::Null,
                    "delivery": "refused",
                    "notice": notice,
                    "execution": exec_facts(&detail.kind),
                }));
            }
            FollowUpDecision::Continue => detail,
        }
    };

    // A settled continuation has no caller-supplied execution identity. Load the
    // predecessor's persisted ResumeContext and carry its primary ref, secondary
    // refs, project/provenance identity, runtime/executor identity, launch mode,
    // and sites without overrides.
    let prior_resume = state
        .state_store
        .get_launch_metadata(&previous.thread_id)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
        .and_then(|metadata| metadata.resume_context)
        .ok_or_else(|| {
            HandlerError::Internal(format!(
                "thread {} has no persisted execution identity",
                previous.thread_id
            ))
        })?;
    ensure_execution_identity_continuity(&previous, &prior_resume)?;
    authorize_execution_refs(
        &prior_resume.item_ref,
        &prior_resume.ref_bindings,
        &ctx,
        &state,
    )?;

    // A rebind that names what the thread already runs with is no change and
    // takes the ordinary continuation below. What remains is authorised like
    // the bindings of a fresh launch before anything is prepared.
    let rebind_changes = ryeos_executor::execution::launch::operator_rebind_changes(
        &state,
        &previous.thread_id,
        &rebind,
    )
    .map_err(build_and_launch_error)?;
    if !rebind_changes.is_empty() {
        authorize_execution_refs(&prior_resume.item_ref, &rebind_changes, &ctx, &state)?;
    }

    use ryeos_engine::contracts::{EffectivePrincipal, Principal};
    let parameters = json!({ "input": input });
    let mut resume_context = prior_resume;
    resume_context.parameters = parameters.clone();
    resume_context.ref_bindings.extend(rebind_changes.clone());
    // Operator authority is re-derived from the verified caller; execution
    // identity itself remains inherited from the predecessor.
    resume_context.requested_by = EffectivePrincipal::Local(Principal {
        fingerprint: ctx.fingerprint.clone(),
        scopes: ctx.scopes.clone(),
    });

    // A rebind that already produced this thread's successor is answered from
    // that successor. Preparing the rebind again would resolve the bound item
    // and read signed configuration as they are now, so a retry could fail or
    // look like a different choice merely because something was edited since.
    if !rebind_changes.is_empty()
        && let Some(successor_id) = previous.successor_thread_id.as_deref()
    {
        let successor = state
            .state_store
            .get_thread(successor_id)
            .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
            .ok_or_else(|| {
                HandlerError::Internal(format!("successor {successor_id} is missing"))
            })?;
        let successor_resume = state
            .state_store
            .get_launch_metadata(successor_id)
            .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
            .and_then(|metadata| metadata.resume_context);
        if successor_resume
            .as_ref()
            .is_some_and(|born| born_from_same_request(born, &resume_context))
        {
            if successor.status == ryeos_state::objects::ThreadStatus::Created.as_str() {
                redrive_created_operator_successor(&state, successor_id).await?;
            }
            return Ok(with_rebind_report(
                json!({
                    "thread_id": successor.thread_id,
                    "delivery": "launched",
                    "notice": Value::Null,
                    "execution": exec_facts(&successor.kind),
                }),
                rebind_report(&rebind, RebindOutcome::Applied(&rebind_changes)),
            ));
        }
        return Ok(with_rebind_report(
            json!({
                "thread_id": successor.thread_id,
                "delivery": "refused",
                "notice": format!(
                    "thread {} already continued as {} by a different input; \
                     follow that thread or start a new chain",
                    previous.thread_id, successor.thread_id
                ),
                "execution": exec_facts(&successor.kind),
            }),
            rebind_report(&rebind, RebindOutcome::Conflict),
        ));
    }

    // The successor preparer restores the predecessor's sealed admitted
    // program and reapplies current caller authorization before birth. Do not
    // re-resolve mutable project bytes here: operator continuation changes the
    // stimulus and principal authority, not the admitted program.
    let candidate_id = ryeos_app::thread_lifecycle::new_thread_id();
    let prepared = if rebind_changes.is_empty() {
        ryeos_executor::execution::launch::prepare_operator_successor_launch(
            &state,
            &candidate_id,
            &resume_context,
            &previous.thread_id,
            &ctx,
        )
        .await
    } else {
        // The changed bindings are prepared again against current signed
        // configuration; everything else stays the admitted program.
        let names = rebind_changes.keys().cloned().collect();
        ryeos_executor::execution::launch::prepare_operator_rebind_successor_launch(
            &state,
            &candidate_id,
            &resume_context,
            &previous.thread_id,
            &ctx,
            &names,
        )
        .await
    };
    let prepared = prepared.map_err(build_and_launch_error)?;
    let mut initial_audit_events = prepared
        .initial_audit_events()
        .map_err(build_and_launch_error)?;
    if let Some(rebound) = prepared.rebound() {
        initial_audit_events.push(launch_binding_rebound_event(rebound));
    }
    // The successor keeps its predecessor's title unless one is supplied.
    let successor_title = match explicit_title {
        Some(title) => Some(title),
        None => state
            .state_store
            .get_facets(&previous.thread_id)
            .map_err(|error| HandlerError::Internal(format!("{error:#}")))?
            .into_iter()
            .find_map(|(key, value)| (key == TITLE_FACET_KEY).then_some(value)),
    };
    if let Some(title) = successor_title {
        initial_audit_events.push(ryeos_app::state_store::NewEventRecord::thread_facet_set(
            TITLE_FACET_KEY,
            &title,
        ));
    }

    let successor_record = ryeos_app::state_store::NewThreadRecord {
        thread_id: candidate_id.clone(),
        chain_root_id: previous.chain_root_id.clone(),
        kind: previous.kind.clone(),
        item_ref: resume_context.item_ref.clone(),
        executor_ref: previous.executor_ref.clone(),
        launch_mode: resume_context.launch_mode.clone(),
        current_site_id: resume_context.current_site_id.clone(),
        origin_site_id: resume_context.origin_site_id.clone(),
        upstream_thread_id: Some(previous.thread_id.clone()),
        requested_by: Some(ctx.fingerprint.clone()),
        project_root: previous.project_root.as_ref().map(std::path::PathBuf::from),
        base_project_snapshot_hash: resume_context
            .project_authority
            .operational_snapshot_projection()
            .map(str::to_owned),
        project_authority: resume_context.project_authority.clone(),
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: None,
    };
    let project_identity = serde_json::to_string(&resume_context.project_context)
        .map_err(|error| HandlerError::Internal(error.to_string()))?;
    let fingerprint = ryeos_app::thread_lifecycle::continuation_request_fingerprint(
        &resume_context.item_ref,
        &resume_context.ref_bindings,
        &project_identity,
        &ctx.fingerprint,
        &ctx.scopes,
        &resume_context.launch_mode,
        &previous.thread_id,
        &parameters,
        &resume_context.project_authority,
        resume_context.lifecycle_authority,
        prepared.launch_metadata().sealed_root_request.as_ref(),
    );

    let outcome = state
        .threads
        .create_or_get_operator_continuation(
            &successor_record,
            &previous.thread_id,
            Some(ryeos_state::queries::ContinuationReasonMarker::OperatorFollowUp.as_str()),
            &fingerprint,
            prepared.launch_metadata(),
            prepared.continuation_transition(),
            initial_audit_events,
        )
        .map_err(|e| HandlerError::Internal(format!("{e:#}")))?;

    use ryeos_app::thread_lifecycle::OperatorContinuation;
    match outcome {
        OperatorContinuation::Created(detail) => {
            let successor_id = detail.thread_id.clone();
            let prepared = prepared.with_persisted_birth_audit();
            let task_id = successor_id.clone();
            let st = (*state).clone();
            let (handoff, ready) = ryeos_executor::execution::launch::LaunchHandoff::channel();
            let task = tokio::spawn(async move {
                ryeos_executor::execution::launch::launch_prepared_operator_successor(
                    st, &task_id, prepared, &handoff,
                )
                .await
            });
            let ready_thread_id = await_operator_handoff(task, ready).await?;
            if ready_thread_id != successor_id {
                return Err(HandlerError::Internal(
                    "operator launch handoff returned a different thread identity".to_string(),
                ));
            }
            Ok(with_rebind_report(
                json!({
                    "thread_id": detail.thread_id,
                    "delivery": "launched",
                    "notice": Value::Null,
                    "execution": exec_facts(&detail.kind),
                }),
                rebind_report(&rebind, RebindOutcome::Applied(&rebind_changes)),
            ))
        }
        OperatorContinuation::Existing(detail) => {
            // Idempotent double-submit → the SAME successor id. If it was stranded
            // `created`, consume the freshly recomputed authority and require the
            // same handoff proof as a newly created successor.
            // The candidate authority named a speculative ID and its snapshot
            // was never committed. A stranded successor is re-driven from its
            // exact durable identity without recapturing its birth snapshot.
            drop(prepared);
            if detail.status == ryeos_state::objects::ThreadStatus::Created.as_str() {
                redrive_created_operator_successor(&state, &detail.thread_id).await?;
            }
            // The same request met its own successor, which was born with
            // exactly these bindings.
            Ok(with_rebind_report(
                json!({
                    "thread_id": detail.thread_id,
                    "delivery": "launched",
                    "notice": Value::Null,
                    "execution": exec_facts(&detail.kind),
                }),
                rebind_report(&rebind, RebindOutcome::Applied(&rebind_changes)),
            ))
        }
        OperatorContinuation::Conflict(detail) => {
            drop(prepared);
            // One predecessor has one successor. A second continuation with
            // another input or another binding choice is this conflict.
            Ok(with_rebind_report(
                json!({
                    "thread_id": detail.thread_id,
                    "delivery": "refused",
                    "notice": format!(
                        "thread {} already continued as {} by a different input; \
                         follow that thread or start a new chain",
                        previous.thread_id, detail.thread_id
                    ),
                    "execution": exec_facts(&detail.kind),
                }),
                rebind_report(&rebind, RebindOutcome::Conflict),
            ))
        }
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:threads/input",
    endpoint: "threads.input",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.threads/input"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned_authority(
        realization: ryeos_state::objects::PinnedProjectRealization,
        capability_ceiling: Vec<String>,
    ) -> ryeos_state::objects::ExecutionProjectAuthority {
        ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "site:test:/project/a".to_string(),
            Some(std::path::PathBuf::from("/project/a")),
            "a".repeat(64),
            realization,
            ryeos_state::objects::EnvironmentAuthority::None,
            capability_ceiling,
        )
        .unwrap()
    }

    fn resume_identity(
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> ryeos_app::launch_metadata::ResumeContext {
        ryeos_app::launch_metadata::ResumeContext {
            kind: "directive_run".to_string(),
            item_ref: "directive:test/identity".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            product_selections: Vec::new(),
            launch_mode: "detached".to_string(),
            parameters: json!({}),
            project_context: ryeos_engine::contracts::ProjectContext::SnapshotHash {
                hash: "a".repeat(64),
            },
            project_authority,
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity: None,
            local_overlay_root: None,
            original_snapshot_hash: Some("a".repeat(64)),
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:origin".to_string(),
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: "user:test".to_string(),
                    scopes: Vec::new(),
                },
            ),
            execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
            scheduled_fire: None,
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
            thread_ceiling: ryeos_state::objects::ThreadWorkloadCeiling::node_root(),
            workspace_input: None,
            evaluator_origin_chain: None,
            executor_ref: Some("executor:test/directive".to_string()),
            runtime_ref: Some("runtime:test/directive".to_string()),
        }
    }

    fn predecessor_identity(
        project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    ) -> ryeos_app::state_store::ThreadDetail {
        ryeos_app::state_store::ThreadDetail {
            thread_id: "T-identity".to_string(),
            chain_root_id: "T-identity".to_string(),
            kind: "directive_run".to_string(),
            status: "completed".to_string(),
            item_ref: "directive:test/identity".to_string(),
            executor_ref: "executor:test/directive".to_string(),
            launch_mode: "detached".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:origin".to_string(),
            upstream_thread_id: None,
            successor_thread_id: None,
            requested_by: Some("user:test".to_string()),
            project_root: project_authority
                .project_root_projection()
                .map(|path| path.display().to_string()),
            project_authority: Some(project_authority),
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
            lifecycle_authority: Some(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            ),
            admitted_launch_capsule_hash: None,
            created_at: "2026-07-24T00:00:00Z".to_string(),
            updated_at: "2026-07-24T00:00:00Z".to_string(),
            started_at: Some("2026-07-24T00:00:00Z".to_string()),
            finished_at: Some("2026-07-24T00:00:01Z".to_string()),
            runtime: ryeos_app::runtime_db::RuntimeInfo::default(),
        }
    }

    #[test]
    fn exact_execution_identity_continuity_accepts_the_same_authority() {
        let authority = pinned_authority(
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            Vec::new(),
        );
        let previous = predecessor_identity(authority.clone());
        let resume = resume_identity(authority);

        ensure_execution_identity_continuity(&previous, &resume).unwrap();
    }

    #[test]
    fn exact_execution_identity_continuity_rejects_requested_by_mismatch() {
        let authority = pinned_authority(
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            Vec::new(),
        );
        let mut previous = predecessor_identity(authority.clone());
        previous.requested_by = Some("user:other".to_string());
        let error = ensure_execution_identity_continuity(&previous, &resume_identity(authority))
            .expect_err("operator continuation must retain the admitted principal");
        match error {
            HandlerError::Structured { code, status, body } => {
                assert_eq!(code, "execution_identity_continuity_mismatch");
                assert_eq!(status, axum::http::StatusCode::CONFLICT.as_u16());
                assert_eq!(body["mismatches"], json!(["requested_by"]));
            }
            other => panic!("expected structured continuity conflict, got {other:?}"),
        }
    }

    #[test]
    fn exact_execution_identity_continuity_rejects_lifecycle_authority_mismatch() {
        let authority = pinned_authority(
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            Vec::new(),
        );
        let mut previous = predecessor_identity(authority.clone());
        previous.lifecycle_authority =
            Some(ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE);
        let error = ensure_execution_identity_continuity(&previous, &resume_identity(authority))
            .expect_err("operator continuation must retain exact lifecycle authority");
        match error {
            HandlerError::Structured { code, status, body } => {
                assert_eq!(code, "execution_identity_continuity_mismatch");
                assert_eq!(status, axum::http::StatusCode::CONFLICT.as_u16());
                assert_eq!(body["mismatches"], json!(["lifecycle_authority"]));
            }
            other => panic!("expected structured continuity conflict, got {other:?}"),
        }
    }

    #[test]
    fn exact_execution_identity_continuity_rejects_authority_only_changes() {
        let authority = pinned_authority(
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            Vec::new(),
        );
        let previous = predecessor_identity(authority.clone());
        let alternatives = [
            authority
                .clone()
                .with_capability_ceiling(vec!["ryeos.execute.tool.test".to_string()])
                .unwrap(),
            pinned_authority(
                ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
                },
                Vec::new(),
            ),
            authority
                .with_child_policy(
                    ryeos_state::objects::ChildProjectAuthorityPolicy::PinAtSpawn {
                        realization: ryeos_state::objects::PinnedChildProjectRealization::ReadOnly,
                    },
                )
                .unwrap(),
        ];

        for alternative in alternatives {
            assert_eq!(
                alternative.project_root_projection(),
                previous
                    .project_authority
                    .as_ref()
                    .and_then(|authority| authority.project_root_projection())
            );
            assert_eq!(
                alternative.operational_snapshot_projection(),
                previous
                    .project_authority
                    .as_ref()
                    .and_then(|authority| authority.operational_snapshot_projection())
            );
            let error =
                ensure_execution_identity_continuity(&previous, &resume_identity(alternative))
                    .unwrap_err();
            match error {
                HandlerError::Structured { code, status, body } => {
                    assert_eq!(code, "execution_identity_continuity_mismatch");
                    assert_eq!(status, axum::http::StatusCode::CONFLICT.as_u16());
                    assert_eq!(
                        body["mismatches"],
                        json!(["project_authority"]),
                        "equal root/snapshot projections must not hide structural authority drift"
                    );
                }
                other => panic!("expected structured continuity conflict, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_thread_target_takes_an_optional_rebind() {
        let request: Request = serde_json::from_value(json!({
            "input": "continue",
            "target": {"kind": "thread", "thread_id": "T-1"},
        }))
        .unwrap();
        match request.target {
            Target::Thread { thread_id, rebind } => {
                assert_eq!(thread_id, "T-1");
                assert!(rebind.is_empty());
            }
            Target::Fresh { .. } => panic!("expected a thread target"),
        }

        let request: Request = serde_json::from_value(json!({
            "input": "continue",
            "target": {
                "kind": "thread",
                "thread_id": "T-1",
                "rebind": {"model": "directive:test/models/second"},
            },
        }))
        .unwrap();
        match request.target {
            Target::Thread { rebind, .. } => assert_eq!(
                rebind,
                BTreeMap::from([(
                    "model".to_string(),
                    "directive:test/models/second".to_string()
                )])
            ),
            Target::Fresh { .. } => panic!("expected a thread target"),
        }

        // A fresh target states its bindings as `ref_bindings`; it has no rebind.
        assert!(
            serde_json::from_value::<Request>(json!({
                "input": "start",
                "target": {
                    "kind": "fresh",
                    "item_ref": "directive:test/item",
                    "project_path": "/project",
                    "ref_bindings": {},
                    "rebind": {"model": "directive:test/models/second"},
                },
            }))
            .is_err()
        );
    }

    #[test]
    fn a_rebind_is_reported_only_when_requested_and_applied_only_on_a_successor() {
        let requested = BTreeMap::from([(
            "model".to_string(),
            "directive:test/models/second".to_string(),
        )]);
        let none = BTreeMap::new();

        // No rebind in the request: the result is exactly what it was.
        let plain = json!({"thread_id": "T-2", "delivery": "launched"});
        for outcome in [
            RebindOutcome::Applied(&none),
            RebindOutcome::ThreadRunning,
            RebindOutcome::Conflict,
        ] {
            assert_eq!(
                with_rebind_report(plain.clone(), rebind_report(&none, outcome)),
                plain
            );
        }

        assert_eq!(
            rebind_report(&requested, RebindOutcome::Applied(&requested)),
            Some(json!({"applied": true, "names": ["model"]}))
        );
        // Requested, but it named what the thread already ran with.
        assert_eq!(
            rebind_report(&requested, RebindOutcome::Applied(&none)),
            Some(json!({"applied": false, "reason": "unchanged"}))
        );
        // Input folded into a running thread: the choice is not applied.
        let folded = with_rebind_report(
            json!({"thread_id": "T-1", "delivery": "submitted", "pending": 1}),
            rebind_report(&requested, RebindOutcome::ThreadRunning),
        );
        assert_eq!(folded["delivery"], "submitted");
        assert_eq!(
            folded["rebind"],
            json!({"applied": false, "reason": "thread_running"})
        );
        assert_eq!(
            rebind_report(&requested, RebindOutcome::Conflict),
            Some(json!({"applied": false, "reason": "conflict"}))
        );
    }

    #[test]
    fn a_rebound_binding_needs_the_execute_capability_on_the_new_ref() {
        let authorizer = ryeos_runtime::authorizer::Authorizer::new();
        let rebind = BTreeMap::from([(
            "model".to_string(),
            "directive:test/models/second".to_string(),
        )]);
        let item = "directive:test/item";
        let item_cap = "ryeos.execute.directive.test/item".to_string();

        let error = authorize_execution_refs_for(
            item,
            &rebind,
            std::slice::from_ref(&item_cap),
            &authorizer,
        )
        .expect_err("the caller cannot execute the new binding");
        match error {
            HandlerError::Forbidden(message) => assert!(
                message.contains("ryeos.execute.directive.test/models/second"),
                "{message}"
            ),
            other => panic!("expected a forbidden refusal, got {other:?}"),
        }

        authorize_execution_refs_for(
            item,
            &rebind,
            &[
                item_cap,
                "ryeos.execute.directive.test/models/second".to_string(),
            ],
            &authorizer,
        )
        .expect("a caller holding both capabilities may rebind");
    }

    #[test]
    fn the_rebound_event_records_each_change_and_the_facts_on_both_sides() {
        let rebound = ryeos_executor::execution::launch_rebind::ReboundLaunch {
            source_thread_id: "T-source".to_string(),
            changes: vec![ryeos_executor::execution::launch_rebind::ReboundBinding {
                name: "model".to_string(),
                from: "directive:test/models/first".to_string(),
                to: "directive:test/models/second".to_string(),
            }],
            from_facts: BTreeMap::from([("model_name".to_string(), json!("first-model"))]),
            to_facts: BTreeMap::from([("model_name".to_string(), json!("second-model"))]),
        };
        let event = launch_binding_rebound_event(&rebound);
        assert_eq!(
            event.event_type,
            ryeos_state::event_types::LAUNCH_BINDING_REBOUND
        );
        assert_eq!(event.storage_class, "indexed");
        assert_eq!(
            event.payload,
            json!({
                "source_thread_id": "T-source",
                "changes": [{
                    "input": "ref_binding",
                    "name": "model",
                    "from": "directive:test/models/first",
                    "to": "directive:test/models/second",
                }],
                "from_facts": {"model_name": "first-model"},
                "to_facts": {"model_name": "second-model"},
            })
        );
    }

    #[test]
    fn a_malformed_rebind_is_a_bad_request() {
        validate_rebind_request(&BTreeMap::new()).unwrap();
        validate_rebind_request(&BTreeMap::from([(
            "model".to_string(),
            "directive:test/models/second".to_string(),
        )]))
        .unwrap();
        // Not a canonical ref.
        let error = validate_rebind_request(&BTreeMap::from([(
            "model".to_string(),
            "not a ref".to_string(),
        )]))
        .expect_err("a value that is not a canonical ref is refused");
        assert!(
            matches!(
                error,
                HandlerError::BadRequest(_) | HandlerError::Structured { .. }
            ),
            "{error:?}"
        );
        // Not a binding name.
        assert!(
            validate_rebind_request(&BTreeMap::from([(
                "not a name".to_string(),
                "directive:test/models/second".to_string(),
            )]))
            .is_err()
        );
    }

    #[test]
    fn a_retried_rebind_is_recognised_by_the_bindings_recorded_at_birth() {
        let authority = pinned_authority(
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            Vec::new(),
        );
        let mut requested = resume_identity(authority);
        requested.parameters = json!({"input": "continue"});
        requested.ref_bindings.insert(
            "model".to_string(),
            "directive:test/models/second".to_string(),
        );
        let born = requested.clone();
        assert!(born_from_same_request(&born, &requested));

        // Another binding choice, another input, or another sender is a
        // different request for the one successor slot.
        let mut other_choice = requested.clone();
        other_choice.ref_bindings.insert(
            "model".to_string(),
            "directive:test/models/third".to_string(),
        );
        let mut no_choice = requested.clone();
        no_choice.ref_bindings.clear();
        let mut other_input = requested.clone();
        other_input.parameters = json!({"input": "something else"});
        let mut other_sender = requested.clone();
        other_sender.requested_by = ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: "user:other".to_string(),
                scopes: Vec::new(),
            },
        );
        for different in [other_choice, no_choice, other_input, other_sender] {
            assert!(!born_from_same_request(&born, &different));
        }
    }

    #[test]
    fn staged_notice_reports_depth() {
        assert_eq!(staged_notice(1), "Input queued (1 staged).");
        assert_eq!(staged_notice(2), "Input queued (2 staged).");
    }

    #[test]
    fn settled_statuses_match_state_store_vocabulary() {
        for status in [
            "completed",
            "failed",
            "cancelled",
            "killed",
            "timed_out",
            "continued",
        ] {
            assert!(is_settled(status), "{status} should be settled");
        }
        for status in ["running", "accepted", "pending"] {
            assert!(!is_settled(status), "{status} should be live");
        }
    }

    #[test]
    fn continuable_set_matches_create_continuation() {
        // Only completed/failed turns can be braided onto (mirrors
        // StateStore::create_continuation). A follow-up to any other settled
        // status must be refused synchronously, not launched.
        for status in ["completed", "failed"] {
            assert!(is_continuable(status), "{status} should be continuable");
        }
        for status in ["continued", "cancelled", "killed", "timed_out", "running"] {
            assert!(!is_continuable(status), "{status} must not be continuable");
        }
    }

    #[test]
    fn continuable_predecessor_continues() {
        // Completed/failed sources braid a successor. An already-continued
        // source is NOT pre-refused here — the create-or-get dedups by fingerprint.
        assert_eq!(
            classify_follow_up("completed", "T-1"),
            FollowUpDecision::Continue
        );
        assert_eq!(
            classify_follow_up("failed", "T-1"),
            FollowUpDecision::Continue
        );
    }

    #[test]
    fn running_source_is_live() {
        // A running thread is no longer refused — it folds operator input live.
        assert_eq!(classify_follow_up("running", "T-1"), FollowUpDecision::Live);
    }

    #[test]
    fn not_yet_running_source_is_refused() {
        // created / accepted / pending have no live loop and no settled turn.
        for status in ["created", "accepted", "pending"] {
            match classify_follow_up(status, "T-1") {
                FollowUpDecision::Refuse(notice) => {
                    assert!(notice.contains("needs a running thread"), "got: {notice}");
                }
                other => panic!("{status} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn settled_non_continuable_source_is_refused() {
        for status in ["cancelled", "killed", "timed_out", "continued"] {
            match classify_follow_up(status, "T-1") {
                FollowUpDecision::Refuse(notice) => {
                    assert!(notice.contains("can only continue"), "got: {notice}");
                }
                other => panic!("{status} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn derived_title_is_the_first_non_empty_line_with_whitespace_collapsed() {
        assert_eq!(
            derive_title("\n  \r\n\t Fix   the\tlogin  bug \nsecond line").as_deref(),
            Some("Fix the login bug")
        );
        assert_eq!(derive_title("one line").as_deref(), Some("one line"));
    }

    #[test]
    fn derived_title_without_any_text_is_none() {
        assert_eq!(derive_title(" \n\t \u{7} \r\n"), None);
    }

    #[test]
    fn derived_title_turns_control_characters_into_separators() {
        assert_eq!(derive_title("a\u{0}b\u{7}\u{7}c").as_deref(), Some("a b c"));
    }

    #[test]
    fn derived_title_cuts_at_80_scalars_and_marks_only_a_cut() {
        let exact = "x".repeat(80);
        assert_eq!(derive_title(&exact).as_deref(), Some(exact.as_str()));
        let long = "x".repeat(81);
        assert_eq!(
            derive_title(&long).as_deref(),
            Some(format!("{}…", "x".repeat(80)).as_str())
        );
    }

    #[test]
    fn derived_title_cuts_on_a_scalar_boundary_for_multibyte_text() {
        // 3-byte and 4-byte scalars: a byte cut at 80 would split one.
        let wide = "界".repeat(100);
        let title = derive_title(&wide).expect("title");
        assert_eq!(title, format!("{}…", "界".repeat(80)));
        let emoji = "😀".repeat(81);
        let title = derive_title(&emoji).expect("title");
        assert_eq!(title.chars().count(), 81);
        assert!(title.starts_with(&"😀".repeat(80)) && title.ends_with('…'));
        // A cut that would leave a trailing space drops it before the mark.
        let spaced = format!("{} {}", "a".repeat(79), "b".repeat(10));
        assert_eq!(
            derive_title(&spaced).as_deref(),
            Some(format!("{}…", "a".repeat(79)).as_str())
        );
    }

    #[test]
    fn an_explicit_title_is_trimmed_and_bounded_in_scalars() {
        assert_eq!(
            validate_title("  Plan the move  ").unwrap(),
            "Plan the move"
        );
        assert_eq!(
            validate_title(&"界".repeat(160)).unwrap().chars().count(),
            160
        );
        for bad in [
            String::new(),
            "   ".to_string(),
            "界".repeat(161),
            "two\nlines".to_string(),
            "bell\u{7}".to_string(),
            "tab\there".to_string(),
        ] {
            assert!(
                matches!(validate_title(&bad), Err(HandlerError::BadRequest(_))),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn the_request_takes_an_optional_title() {
        let target = json!({"kind": "thread", "thread_id": "T-1"});
        let without: Request =
            serde_json::from_value(json!({"input": "hi", "target": target})).unwrap();
        assert!(without.title.is_none());
        let with: Request =
            serde_json::from_value(json!({"input": "hi", "target": target, "title": "A title"}))
                .unwrap();
        assert_eq!(with.title.as_deref(), Some("A title"));
    }
}
