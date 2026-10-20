//! `ui.ryeos.threads.list` and `ui.ryeos.thread.inspect` — thread
//! listing and read-only inspection for the ryeos-ui.
//!
//! Locates bounded thread-list candidates in the projection, then checks each
//! disclosed row against signed thread authority and the exact caller scope.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use base64::Engine;
use lillux::crypto::Signature;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_app::state_store::AuthoritativeThreadSubject;
use ryeos_engine::contracts::ProjectContext;
use ryeos_executor::executor::ServiceAvailability;

fn default_limit() -> usize {
    100
}

const MAX_THREAD_LIST_LIMIT: usize = 2_000;
pub(crate) const MAX_THREAD_LIST_CANDIDATES_SCANNED: usize = 8_192;
static THREAD_LIST_CALLS: AtomicU64 = AtomicU64::new(0);
static THREAD_LIST_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn collect_authorized_thread_items(
    limit: usize,
    sort: ryeos_state::queries::ThreadSort,
    mut page: impl FnMut(
        usize,
        Option<&ryeos_state::queries::ThreadListCursor>,
    ) -> Result<Vec<ryeos_app::state_store::ThreadListItem>>,
    mut authorize: impl FnMut(&ryeos_app::state_store::ThreadListItem) -> Result<bool>,
) -> Result<(Vec<ryeos_app::state_store::ThreadListItem>, usize)> {
    let mut cursor: Option<ryeos_state::queries::ThreadListCursor> = None;
    let mut scanned = 0usize;
    let mut authorized = Vec::with_capacity(limit);
    while authorized.len() < limit && scanned < MAX_THREAD_LIST_CANDIDATES_SCANNED {
        let page_limit = ryeos_state::queries::MAX_THREAD_CANDIDATE_PAGE
            .min(MAX_THREAD_LIST_CANDIDATES_SCANNED - scanned);
        let candidates = page(page_limit, cursor.as_ref())?;
        let count = candidates.len();
        if count > page_limit {
            anyhow::bail!("thread candidate reader exceeded its bounded page");
        }
        if count == 0 {
            break;
        }
        for item in candidates {
            scanned += 1;
            let next_cursor = ryeos_state::queries::ThreadListCursor::from_position(
                &item.created_at,
                &item.thread_id,
                &item.status,
            );
            if cursor.as_ref().is_some_and(|previous| {
                !thread_candidate_cursor_advances(previous, &next_cursor, sort)
            }) {
                anyhow::bail!("thread candidate page did not advance its ordered locator");
            }
            cursor = Some(next_cursor);
            if authorize(&item)? {
                authorized.push(item);
                if authorized.len() == limit {
                    break;
                }
            }
        }
        if count < page_limit {
            break;
        }
    }
    Ok((authorized, scanned))
}

fn thread_candidate_cursor_advances(
    previous: &ryeos_state::queries::ThreadListCursor,
    next: &ryeos_state::queries::ThreadListCursor,
    sort: ryeos_state::queries::ThreadSort,
) -> bool {
    let previous_key = (&previous.created_at, &previous.thread_id);
    let next_key = (&next.created_at, &next.thread_id);
    match sort {
        ryeos_state::queries::ThreadSort::Default => next_key > previous_key,
        ryeos_state::queries::ThreadSort::Newest => next_key < previous_key,
        ryeos_state::queries::ThreadSort::Watch => {
            (next.watch_terminal & !previous.watch_terminal)
                || (next.watch_terminal == previous.watch_terminal && next_key < previous_key)
        }
    }
}

struct ThreadListInFlightGuard;

impl Drop for ThreadListInFlightGuard {
    fn drop(&mut self) {
        THREAD_LIST_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Read an optional string filter param: absent, non-string, or empty/blank all
/// mean "unfiltered" (`None`). The client sends `""` for an unset filter facet,
/// so this is where that collapses to no filter.
fn string_filter(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn string_list_filter(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn active_filter(params: &Value) -> bool {
    match params.get("active") {
        Some(Value::Bool(active)) => *active,
        Some(Value::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "active" | "live" | "running"
        ),
        _ => false,
    }
}

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let started = Instant::now();
    let call = THREAD_LIST_CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let in_flight = THREAD_LIST_IN_FLIGHT.fetch_add(1, Ordering::Relaxed) + 1;
    let _in_flight_guard = ThreadListInFlightGuard;
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(default_limit)
        .clamp(1, MAX_THREAD_LIST_LIMIT);

    // Optional ordering: the watch dashboard requests `sort: watch`
    // (active-first, then newest), `newest` is newest-first; anything else
    // keeps the default order.
    let sort = match params.get("sort").and_then(|v| v.as_str()) {
        Some("watch") => ryeos_app::thread_lifecycle::ThreadSort::Watch,
        Some("newest") => ryeos_app::thread_lifecycle::ThreadSort::Newest,
        _ => ryeos_app::thread_lifecycle::ThreadSort::Default,
    };

    // Optional dashboard filters only narrow candidates. The compiled
    // attachment's project or principal scope is independent of those params;
    // an empty facet never upgrades a browser seat to node-wide authority.
    let attachment_project = caller.project_query_identity()?;
    let (principal, project_root) = match &caller {
        crate::seat_auth::SeatCaller::Attachment(_) => match attachment_project {
            Some(root) => (None, Some(root)),
            None => (Some(caller.principal_id().to_string()), None),
        },
        crate::seat_auth::SeatCaller::Operator { .. } => (None, project_filter(&params, None)?),
    };
    let filter = ryeos_app::thread_lifecycle::ThreadListFilter {
        principal,
        status: string_filter(&params, "status"),
        kind: string_filter(&params, "kind"),
        requested_by: string_filter(&params, "requested_by"),
        facet: string_filter(&params, "facet_key").zip(string_filter(&params, "facet_value")),
        // `active` narrows to live (non-terminal) threads. The filter accepts
        // bools for authored views and text for the TUI live-filter input.
        active_only: active_filter(&params),
        exclude_item_prefixes: string_list_filter(&params, "exclude_item_prefixes"),
        project_root,
    };

    // The projection supplies bounded locators, not read authority. Fetch
    // raw candidates without continuation/follow enrichment, then authorize
    // each exact signed subject before any display decoration. A foreign or
    // stale first page must not crowd later authorized candidates out of the
    // requested page. The fixed scan budget prevents unbounded work; neither
    // an exhausted budget nor an empty projection page proves absence.
    let (authorized_items, scanned) = collect_authorized_thread_items(
        limit,
        sort,
        |page_limit, cursor| {
            state
                .state_store
                .list_threads_candidate_page(page_limit, &filter, sort, cursor)
        },
        |item| {
            let signed = match crate::thread_authorization::authorize_exact_thread_subjects(
                &ctx,
                &state,
                &caller,
                &[&item.thread_id],
            ) {
                Ok(mut subjects) => subjects.pop().expect("one subject requested"),
                Err(HandlerError::NotFound) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if !listed_thread_matches_signed_subject(item, &signed) {
                return Err(HandlerError::NotFound.into());
            }
            Ok(true)
        },
    )?;
    let mut threads = state
        .threads
        .display_verified_thread_list_items(authorized_items);
    for row in &mut threads {
        row.project = row.item.project_root.as_deref().and_then(|root| {
            let path = Path::new(root);
            Some(ryeos_app::thread_lifecycle::ProjectSummary {
                path: root.to_string(),
                name: path.file_name()?.to_str()?.to_string(),
            })
        });
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    if elapsed_ms >= 250 || in_flight >= 4 {
        tracing::warn!(
            call,
            in_flight,
            elapsed_ms,
            rows = threads.len(),
            limit,
            scanned,
            "slow or overlapping ryeos-ui thread-list reconciliation"
        );
    } else {
        tracing::debug!(
            call,
            in_flight,
            elapsed_ms,
            rows = threads.len(),
            limit,
            scanned,
            "ryeos-ui thread-list reconciliation"
        );
    }

    Ok(serde_json::json!({
        "threads": threads,
        "coverage": {"state":"observed", "reason":"projection_locator_not_exhaustive"},
    }))
}

pub(crate) fn listed_thread_matches_signed_subject(
    item: &ryeos_app::state_store::ThreadListItem,
    signed: &AuthoritativeThreadSubject,
) -> bool {
    item.thread_id == signed.thread_id
        && item.chain_root_id == signed.chain_root_id
        && item.kind == signed.kind
        && item.item_ref == signed.item_ref
        && item.launch_mode == signed.launch_mode
        && item.current_site_id == signed.current_site_id
        && item.origin_site_id == signed.origin_site_id
        && item.upstream_thread_id == signed.upstream_thread_id
        && item.project_root.as_deref().map(Path::new) == signed.project_root.as_deref()
        && item.created_at == signed.created_at
        && item.updated_at == signed.updated_at
        && item.status == signed.status.as_str()
        && item.requested_by == signed.requested_by
        && item.project_authority.as_ref() == Some(&signed.project_authority)
        && item.admitted_launch_capsule_hash == signed.admitted_launch_capsule_hash
}

fn project_filter(params: &Value, caller_project_root: Option<&Path>) -> Result<Option<PathBuf>> {
    match string_filter(params, "project").as_deref() {
        Some("current") => Ok(caller_project_root.map(Path::to_path_buf)),
        Some(path) => canonicalize_existing_dir(path).map(Some),
        None => Ok(None),
    }
}

fn canonicalize_existing_dir(path: &str) -> Result<PathBuf> {
    let canonical = Path::new(path).canonicalize()?;
    Ok(canonical)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub thread_id: String,
    #[serde(default = "default_event_limit")]
    pub event_limit: usize,
}

fn default_event_limit() -> usize {
    100
}

/// The signed read budgets of thread inspection.
pub(crate) fn inspection_policy(
    state: &AppState,
) -> Result<&ryeos_node_runtime::node_policy::sections::ui_inspection::UiInspectionPolicy> {
    state
        .node_policy
        .require::<ryeos_node_runtime::node_policy::sections::ui_inspection::UiInspectionPolicy>()
}

fn completed_signed_facets(
    facets: BTreeMap<String, String>,
    exhausted: bool,
) -> Option<BTreeMap<String, String>> {
    exhausted.then_some(facets)
}

pub(crate) fn fold_inspection_facet_event(
    facets: &mut BTreeMap<String, String>,
    event_type: &str,
    durability: ryeos_state::objects::EventDurability,
    payload: &Value,
) -> bool {
    if event_type != ryeos_state::event_types::THREAD_FACET_SET
        || !durability.is_projection_indexed()
    {
        return true;
    }
    super::ui_field_execution::fold_signed_facet(facets, payload).is_ok()
}

/// Only a fully exhausted fixed-anchor history proves the facet map, including
/// an empty map. The signed reader returns newest-first events.
pub(crate) fn read_signed_inspection_facets(
    state: &AppState,
    signed: &AuthoritativeThreadSubject,
) -> Result<Option<BTreeMap<String, String>>> {
    let Some(anchor) = state
        .state_store
        .capture_thread_event_read_anchor(&signed.chain_root_id, &signed.thread_id)?
    else {
        return Ok(None);
    };
    let budgets = inspection_policy(state)?;
    let mut progress = None;
    let mut facets = BTreeMap::new();
    for _ in 0..budgets.max_facet_pages_per_thread {
        let page = state.state_store.read_thread_event_page(
            &anchor,
            progress.as_ref(),
            u64::from(budgets.max_facet_page_events),
            budgets.max_facet_page_bytes,
        )?;
        if page.events.is_empty() && page.progress.is_some() {
            anyhow::bail!("signed thread facet history made no progress");
        }
        for (_, event) in page.events {
            // The projection ignores malformed facet payloads. Preserve
            // independently verified thread facts, but do not claim a
            // complete facet map when signed testimony cannot be folded.
            if !fold_inspection_facet_event(
                &mut facets,
                &event.event_type,
                event.durability,
                &event.payload,
            ) {
                return Ok(None);
            }
        }
        match page.progress {
            Some(next) => progress = Some(next),
            None => return Ok(completed_signed_facets(facets, true)),
        }
    }
    Ok(completed_signed_facets(facets, false))
}

pub async fn handle_inspect(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let req: InspectRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let signed = crate::thread_authorization::authorize_exact_thread_subjects(
        &ctx,
        &state,
        &caller,
        &[&req.thread_id],
    )?
    .pop()
    .expect("one exact thread authorization returns one signed subject");

    let Some(mut thread) = state.threads.get_thread_view(&req.thread_id)? else {
        return Err(HandlerError::NotFound.into());
    };
    let displayed = &thread.thread;
    if displayed.thread_id != signed.thread_id
        || displayed.chain_root_id != signed.chain_root_id
        || displayed.kind != signed.kind
        || displayed.item_ref != signed.item_ref
        || displayed.executor_ref != signed.executor_ref
        || displayed.launch_mode != signed.launch_mode
        || displayed.current_site_id != signed.current_site_id
        || displayed.origin_site_id != signed.origin_site_id
        || displayed.upstream_thread_id != signed.upstream_thread_id
        || displayed.project_root.as_deref().map(Path::new) != signed.project_root.as_deref()
        || displayed.created_at != signed.created_at
        || displayed.updated_at != signed.updated_at
        || displayed.started_at != signed.started_at
        || displayed.finished_at != signed.finished_at
        || displayed.status != signed.status.as_str()
        || displayed.admitted_launch_capsule_hash != signed.admitted_launch_capsule_hash
        || displayed.result_project_snapshot_hash != signed.result_project_snapshot_hash
        || displayed.result_workspace_output_capture_hash
            != signed.result_workspace_output_capture_hash
        || displayed.requested_by != signed.requested_by
        || displayed.project_authority.as_ref() != Some(&signed.project_authority)
    {
        return Err(HandlerError::NotFound.into());
    }
    // The live waiter and projection fallback are not signed follow evidence.
    // Do not let the serialized ThreadView bypass the explicit omission below.
    thread.follow = None;
    // The successor is projected from continuation history. Until this read
    // authenticates that edge, do not expose it as verified inspection data.
    thread.thread.successor_thread_id = None;
    // Runtime, lifecycle metadata, project display and staged-input depth are
    // operational projections. Do not launder them through the verified
    // thread object; separately named operational fields retain their own
    // coverage and must not be interpreted as signed execution testimony.
    thread.thread.runtime = Default::default();
    thread.thread.lifecycle_authority = None;
    thread.project = None;
    thread.pending = 0;

    let result = state.state_store.get_thread_result_authoritative(
        &signed.chain_root_id,
        &req.thread_id,
        signed.status.as_str(),
    )?;
    let outcome_coverage = outcome_coverage(signed.status, result.is_some());
    let artifacts = state.threads.list_thread_artifacts(&req.thread_id)?;
    // These collections have only projection/operational readers in this
    // service. An empty list is not proof of absence; publish explicit
    // coverage until each has a signed positive/exhaustive read contract.
    let children = Vec::<Value>::new();
    let signed_facets = read_signed_inspection_facets(&state, &signed)?;
    let facets_map = signed_facets.clone().unwrap_or_default();
    let event_page = state.state_store.recent_thread_events(
        &signed.chain_root_id,
        &req.thread_id,
        None,
        req.event_limit
            .clamp(1, inspection_policy(&state)?.max_event_limit as usize),
        2 * 1024 * 1024,
    )?;
    let events = state
        .state_store
        .authenticate_execution_event_candidates(&signed.chain_root_id, &event_page.events)?;

    let usage = Value::Array(Vec::new());

    // Staged operator-input depth (05c.1): lets the input area show an "N staged"
    // chip without a separate service call.
    let pending = state.live_input.pending_len(&req.thread_id);

    // Durable execution posture (05b.v2): preserve an explicit coverage result;
    // an absent, unreadable, or corrupt audit record is not a successful empty
    // projection.
    let thread_meta = read_thread_meta(&state, &signed.thread_id, &signed.item_ref);
    let execution = execution_meta_rows(thread_meta.value(), &thread.thread.executor_ref);
    let execution_coverage = thread_meta.coverage();

    // No signed follow-state fold exists here. The empty value is paired with
    // explicit unavailable coverage, not a claim that no follow fact exists.
    let follow = Value::Array(Vec::new());
    let evidence_coverage = [
        ("children", "signed_child_collection_not_proven"),
        ("usage", "signed_chain_usage_not_available"),
        ("follow", "signed_follow_state_not_available"),
        ("successor", "signed_continuation_edge_not_available"),
        ("runtime", "operational_projection_not_in_signed_inspection"),
        (
            "project_summary",
            "display_projection_not_in_signed_inspection",
        ),
    ]
    .map(|(subject, reason)| {
        serde_json::json!({
            "subject": subject,
            "state": "unavailable",
            "reason": reason,
        })
    });
    let mut evidence_coverage = evidence_coverage.to_vec();
    evidence_coverage.push(match signed_facets {
        Some(_) => serde_json::json!({"subject": "facets", "state": "complete"}),
        None => serde_json::json!({
            "subject": "facets",
            "state": "unavailable",
            "reason": "signed_facet_map_not_proven",
        }),
    });

    let mut response = serde_json::json!({
        "schema_version": "ryeos-ui.thread.inspect.v1",
        "thread": thread,
        "result": result,
        "outcome_coverage": outcome_coverage,
        "artifacts": artifacts,
        "children": children,
        "facets": facets_map,
        "events": events,
        "events_truncated": event_page.has_more,
        "usage": usage,
        "pending": pending,
        "execution_meta": execution,
        "execution_coverage": [execution_coverage],
        "follow": follow,
        "evidence_coverage": evidence_coverage,
    });
    if let Some(meta) = thread_meta.into_value() {
        response
            .as_object_mut()
            .expect("inspect response is an object")
            .insert("thread_meta".to_string(), meta);
    }
    Ok(response)
}

/// The result owner returns None only when the authenticated snapshot contains
/// no result, error, or outcome code. Never infer success from terminal status.
fn outcome_coverage(status: ryeos_state::objects::ThreadStatus, has_result: bool) -> Value {
    let (state, reason) = if has_result {
        ("available", "signed_outcome_fields_retained")
    } else if status.is_terminal() {
        ("absent", "terminal_snapshot_declares_no_outcome_fields")
    } else {
        ("pending", "nonterminal_snapshot_has_no_outcome_fields_yet")
    };
    serde_json::json!({"state": state, "reason": reason, "thread_status": status.as_str()})
}

#[derive(Debug)]
enum ThreadMetaRead {
    Available(Value),
    Unavailable(&'static str),
}

impl ThreadMetaRead {
    fn value(&self) -> Option<&Value> {
        match self {
            Self::Available(value) => Some(value),
            Self::Unavailable(_) => None,
        }
    }

    fn into_value(self) -> Option<Value> {
        match self {
            Self::Available(value) => Some(value),
            Self::Unavailable(_) => None,
        }
    }

    fn coverage(&self) -> Value {
        match self {
            Self::Available(_) => {
                serde_json::json!({"state":"available","reason":"audit_record_retained"})
            }
            Self::Unavailable(reason) => {
                serde_json::json!({"state":"unavailable","reason":reason})
            }
        }
    }
}

/// Read the thread's signed `thread.json` audit record while retaining why its
/// durable posture cannot be projected.
fn read_thread_meta(state: &AppState, thread_id: &str, item_ref: &str) -> ThreadMetaRead {
    let meta = match state.state_store.get_launch_metadata(thread_id) {
        Ok(Some(meta)) => meta,
        Ok(None) => return ThreadMetaRead::Unavailable("launch_metadata_absent"),
        Err(_) => return ThreadMetaRead::Unavailable("launch_metadata_unreadable"),
    };
    let Some(ctx) = meta.resume_context.as_ref() else {
        return ThreadMetaRead::Unavailable("resume_context_absent");
    };
    let project_root = match &ctx.project_context {
        ProjectContext::LocalPath { path } => path,
        _ => return ThreadMetaRead::Unavailable("project_context_not_local"),
    };
    let path = project_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("threads")
        .join(thread_id)
        .join("thread.json");
    let signed = match std::fs::read_to_string(&path) {
        Ok(signed) => signed,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ThreadMetaRead::Unavailable("audit_record_missing");
        }
        Err(_) => return ThreadMetaRead::Unavailable("audit_record_unreadable"),
    };
    match verify_thread_meta_document(&signed, &state.identity) {
        Ok(value) if audit_record_matches_subject(&value, thread_id, item_ref) => {
            ThreadMetaRead::Available(value)
        }
        Ok(_) => ThreadMetaRead::Unavailable("audit_record_subject_mismatch"),
        Err(reason) => ThreadMetaRead::Unavailable(reason),
    }
}

fn audit_record_matches_subject(value: &Value, thread_id: &str, item_ref: &str) -> bool {
    value.get("thread_id").and_then(Value::as_str) == Some(thread_id)
        && value.get("item_ref").and_then(Value::as_str) == Some(item_ref)
}

/// Verify the exact node-signed audit document before its body contributes
/// execution posture. Merely parsing the body would let a tampered or
/// predecessor-node record masquerade as current durable evidence.
fn verify_thread_meta_document(
    signed: &str,
    identity: &ryeos_node_runtime::identity::NodeIdentity,
) -> std::result::Result<Value, &'static str> {
    let (body, header) =
        lillux::signature::strip_canonical_signature_with_envelope(signed, "#", None, false)
            .map_err(|_| "audit_record_signature_invalid")?;
    let header = header.ok_or("audit_record_signature_missing")?;
    if header.signer_fingerprint != identity.fingerprint()
        || lillux::sha256_hex(body.as_bytes()) != header.content_hash
    {
        return Err("audit_record_signature_invalid");
    }
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(&header.signature_b64)
        .map_err(|_| "audit_record_signature_invalid")?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| "audit_record_signature_invalid")?;
    identity
        .verify_hash(&header.content_hash, &signature)
        .map_err(|_| "audit_record_signature_invalid")?;
    serde_json::from_str::<Value>(&body).map_err(|_| "audit_record_corrupt")
}

/// Build the Execution section's labeled-metric rows (the same `{label, value}`
/// shape the Usage section projects) from durable data: the `thread.json` audit
/// record for caps/limits/trust/model, and the executor reference already
/// compared with the signed thread subject for runtime identity. Rows with no
/// value are dropped, so a thread with no audit record yields just the runtime
/// row (or nothing).
fn execution_meta_rows(thread_meta: Option<&Value>, executor_ref: &str) -> Value {
    fn row(label: &str, value: String) -> Option<Value> {
        (!value.is_empty()).then(|| serde_json::json!({ "label": label, "value": value }))
    }

    let mut rows: Vec<Value> = Vec::new();
    if let Some(meta) = thread_meta {
        if let Some(caps) = meta.get("capabilities").and_then(|v| v.as_array()) {
            rows.extend(row("capabilities minted", caps.len().to_string()));
        }
        if let Some(limits) = meta.get("limits").filter(|v| !v.is_null()) {
            rows.extend(row("limits", compact_limits(limits)));
        }
        if let Some(trust) = meta.get("effective_trust_class").and_then(|v| v.as_str()) {
            rows.extend(row("trust class", trust.to_string()));
        }
        if let Some(model) = meta.get("model").and_then(|v| v.as_str()) {
            rows.extend(row("model", model.to_string()));
        }
    }
    // The caller compared this executor reference with the signed subject
    // before invoking this formatter; it remains available without thread.json.
    rows.extend(row("runtime", executor_ref.to_string()));

    Value::Array(rows)
}

/// Build the Follow section's labeled-metric rows (`{label, value}`, the shape
/// the Usage / Execution sections project) from a thread's follow-lineage fact.
/// `None` (a non-follow thread) yields an empty array, so the detail lens's
/// Follow section reads empty rather than dumping the `thread.follow` object.
/// Absent optional fields (e.g. `child_terminal_status` while the child still
/// runs, or the child identities on a durably-recognized successor) drop their
/// row rather than showing a blank.
#[cfg(test)]
fn follow_rows(follow: Option<&ryeos_app::thread_lifecycle::FollowFact>) -> Value {
    fn row(label: &str, value: Option<String>) -> Option<Value> {
        value
            .filter(|v| !v.is_empty())
            .map(|value| serde_json::json!({ "label": label, "value": value }))
    }

    let mut rows: Vec<Value> = Vec::new();
    if let Some(f) = follow {
        // `state` (display_state) heads the section: the operator-legible
        // "suspended" / "resumed" the row tones off, distinct from the raw role.
        rows.extend(row("state", Some(f.display_state.to_string())));
        rows.extend(row("phase", f.phase.clone()));
        rows.extend(row("follow node", f.follow_node.clone()));
        rows.extend(row("child chain", f.child_chain_root_id.clone()));
        rows.extend(row("child thread", f.child_thread_id.clone()));
        rows.extend(row("child status", f.child_terminal_status.clone()));
        rows.extend(row(
            "cohort progress",
            f.cohort
                .as_ref()
                .map(|cohort| format!("{} done / {} expected", cohort.done, cohort.expected)),
        ));
        rows.extend(row(
            "resume successor",
            f.parent_successor_thread_id.clone(),
        ));
    }
    Value::Array(rows)
}

/// Render a HardLimits-shaped limits object as a compact `key=value` line for a
/// single projected row. A non-object (or empty) limits value renders as its
/// compact JSON.
fn compact_limits(limits: &Value) -> String {
    match limits.as_object() {
        Some(map) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| format!("{k}={}", compact_scalar(v)))
            .collect::<Vec<_>>()
            .join(", "),
        _ => limits.to_string(),
    }
}

fn compact_scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/threads/list",
    endpoint: "ui.ryeos.threads.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

pub const INSPECT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/thread/inspect",
    endpoint: "ui.ryeos.thread.inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle_inspect(params, ctx, state).await }),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_coverage_distinguishes_every_terminal_status_without_inventing_success() {
        use ryeos_state::objects::ThreadStatus;
        for status in [ThreadStatus::Created, ThreadStatus::Running] {
            let coverage = outcome_coverage(status, false);
            assert_eq!(coverage["state"], "pending");
            assert_eq!(
                coverage["reason"],
                "nonterminal_snapshot_has_no_outcome_fields_yet"
            );
            assert_eq!(coverage["thread_status"], status.as_str());
        }
        for status in [
            ThreadStatus::Completed,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            let coverage = outcome_coverage(status, false);
            assert_eq!(coverage["state"], "absent");
            assert_eq!(
                coverage["reason"],
                "terminal_snapshot_declares_no_outcome_fields"
            );
            assert_eq!(coverage["thread_status"], status.as_str());
        }
        // A signed record may carry only error or outcome_code, not a result
        // payload. Availability describes fields, not successful completion.
        for status in [
            ThreadStatus::Created,
            ThreadStatus::Running,
            ThreadStatus::Completed,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            let coverage = outcome_coverage(status, true);
            assert_eq!(coverage["state"], "available");
            assert_eq!(coverage["reason"], "signed_outcome_fields_retained");
            assert_eq!(coverage["thread_status"], status.as_str());
            assert!(coverage.get("outcome_code").is_none());
        }
    }

    #[test]
    fn complete_signed_history_proves_an_empty_facet_map() {
        assert_eq!(
            completed_signed_facets(BTreeMap::new(), true),
            Some(BTreeMap::new())
        );
    }

    #[test]
    fn incomplete_signed_history_does_not_publish_partial_facets() {
        let mut partial = BTreeMap::new();
        partial.insert("phase".to_string(), "new".to_string());
        assert_eq!(completed_signed_facets(partial, false), None);
    }

    #[test]
    fn malformed_signed_facet_cannot_be_folded() {
        let mut facets = BTreeMap::new();
        assert!(!fold_inspection_facet_event(
            &mut facets,
            ryeos_state::event_types::THREAD_FACET_SET,
            ryeos_state::objects::EventDurability::Durable,
            &serde_json::json!({"key": "phase", "value": 7}),
        ));
        assert!(facets.is_empty());
    }

    #[test]
    fn inspection_facets_keep_latest_durable_value_and_ignore_journal() {
        use ryeos_state::objects::EventDurability;
        let mut facets = BTreeMap::new();
        for (durability, value) in [
            (EventDurability::Durable, "new"),
            (EventDurability::Journal, "journal"),
            (EventDurability::Durable, "old"),
        ] {
            assert!(fold_inspection_facet_event(
                &mut facets,
                ryeos_state::event_types::THREAD_FACET_SET,
                durability,
                &serde_json::json!({"key": "phase", "value": value}),
            ));
        }
        assert_eq!(facets.get("phase").map(String::as_str), Some("new"));
    }

    fn candidate(index: usize) -> ryeos_app::state_store::ThreadListItem {
        ryeos_app::state_store::ThreadListItem {
            thread_id: format!("T-{index:05}"),
            chain_root_id: format!("T-{index:05}"),
            kind: "tool".into(),
            status: "running".into(),
            item_ref: "tool:example".into(),
            launch_mode: "wait".into(),
            current_site_id: "site:host".into(),
            origin_site_id: "site:host".into(),
            upstream_thread_id: None,
            successor_thread_id: None,
            requested_by: None,
            project_root: None,
            project_authority: None,
            lifecycle_authority: None,
            admitted_launch_capsule_hash: None,
            created_at: "same-time".into(),
            updated_at: "same-time".into(),
        }
    }

    #[test]
    fn signed_first_thread_list_refills_after_foreign_first_page() {
        let (items, scanned) = collect_authorized_thread_items(
            2,
            ryeos_state::queries::ThreadSort::Default,
            |page_limit, after| {
                let start = after
                    .map(|cursor| cursor.thread_id[2..].parse::<usize>().unwrap() + 1)
                    .unwrap_or(0);
                Ok((start..(start + page_limit).min(300))
                    .map(candidate)
                    .collect())
            },
            |item| Ok(item.thread_id == "T-00258" || item.thread_id == "T-00259"),
        )
        .unwrap();
        assert_eq!(scanned, 260);
        assert_eq!(
            items
                .iter()
                .map(|item| item.thread_id.as_str())
                .collect::<Vec<_>>(),
            ["T-00258", "T-00259"]
        );
    }

    #[test]
    fn signed_first_thread_list_has_a_fixed_foreign_candidate_scan_budget() {
        let (items, scanned) = collect_authorized_thread_items(
            1,
            ryeos_state::queries::ThreadSort::Default,
            |page_limit, after| {
                let start = after
                    .map(|cursor| cursor.thread_id[2..].parse::<usize>().unwrap() + 1)
                    .unwrap_or(0);
                Ok((start..start + page_limit).map(candidate).collect())
            },
            |_| Ok(false),
        )
        .unwrap();
        assert!(items.is_empty());
        assert_eq!(scanned, MAX_THREAD_LIST_CANDIDATES_SCANNED);
    }

    #[test]
    fn candidate_cursor_must_advance_in_the_selected_sort_order() {
        use ryeos_state::queries::{ThreadListCursor, ThreadSort};
        let first = ThreadListCursor::from_position("time-a", "T-a", "running");
        let later = ThreadListCursor::from_position("time-a", "T-b", "running");
        assert!(thread_candidate_cursor_advances(
            &first,
            &later,
            ThreadSort::Default
        ));
        assert!(!thread_candidate_cursor_advances(
            &first,
            &later,
            ThreadSort::Newest
        ));
        let terminal = ThreadListCursor::from_position("time-z", "T-z", "completed");
        assert!(thread_candidate_cursor_advances(
            &first,
            &terminal,
            ThreadSort::Watch
        ));
        assert!(!thread_candidate_cursor_advances(
            &terminal,
            &first,
            ThreadSort::Watch
        ));
    }

    #[test]
    fn current_project_filter_uses_retained_canonical_identity() {
        let retained = Path::new("/canonical/project");
        let filter = project_filter(
            &serde_json::json!({
                "project": "current",
                "project_path": "/untrusted/alternate"
            }),
            Some(retained),
        )
        .expect("current project filter");
        assert_eq!(filter.as_deref(), Some(retained));
    }

    #[test]
    fn projectless_current_filter_does_not_infer_a_project() {
        assert_eq!(
            project_filter(
                &serde_json::json!({
                    "project": "current",
                    "project_path": "/untrusted/browser/path"
                }),
                None,
            )
            .expect("projectless filter"),
            None
        );
    }

    #[test]
    fn listed_status_and_item_must_match_signed_subject() {
        use ryeos_app::state_store::ThreadListItem;
        use ryeos_app::thread_lifecycle::{ExecutionFacts, ThreadListView};
        use ryeos_state::objects::{ExecutionProjectAuthority, ThreadStatus};

        let authority = ExecutionProjectAuthority::PROJECTLESS;
        let signed = AuthoritativeThreadSubject {
            thread_id: "T-listed".into(),
            chain_root_id: "T-listed".into(),
            kind: "tool".into(),
            item_ref: "tool:example".into(),
            executor_ref: "native:tool".into(),
            launch_mode: "wait".into(),
            current_site_id: "site:host".into(),
            origin_site_id: "site:host".into(),
            upstream_thread_id: None,
            project_root: None,
            created_at: "created".into(),
            updated_at: "updated".into(),
            started_at: None,
            finished_at: None,
            status: ThreadStatus::Running,
            requested_by: Some("operator".into()),
            project_authority: authority.clone(),
            admitted_launch_capsule_hash: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        };
        let mut row = ThreadListView {
            item: ThreadListItem {
                thread_id: signed.thread_id.clone(),
                chain_root_id: signed.chain_root_id.clone(),
                kind: signed.kind.clone(),
                status: signed.status.as_str().into(),
                item_ref: signed.item_ref.clone(),
                launch_mode: signed.launch_mode.clone(),
                current_site_id: signed.current_site_id.clone(),
                origin_site_id: signed.origin_site_id.clone(),
                upstream_thread_id: None,
                successor_thread_id: None,
                requested_by: signed.requested_by.clone(),
                project_root: None,
                project_authority: Some(authority),
                lifecycle_authority: None,
                admitted_launch_capsule_hash: None,
                created_at: signed.created_at.clone(),
                updated_at: signed.updated_at.clone(),
            },
            execution: ExecutionFacts {
                supports_continuation: false,
                supports_operator_followup: false,
            },
            follow: None,
            project: None,
            pending: 0,
            facets: Default::default(),
            current_node: None,
            error: None,
        };
        assert!(listed_thread_matches_signed_subject(&row.item, &signed));
        row.item.status = "completed".into();
        assert!(!listed_thread_matches_signed_subject(&row.item, &signed));
        row.item.status = signed.status.as_str().into();
        row.item.item_ref = "tool:other".into();
        assert!(!listed_thread_matches_signed_subject(&row.item, &signed));
    }

    #[test]
    fn execution_meta_from_full_audit_record() {
        let meta = serde_json::json!({
            "capabilities": ["ryeos.execute.tool.a", "ryeos.execute.tool.b"],
            "limits": { "turns": 25, "wall_secs": 600 },
            "effective_trust_class": "trusted_bundle",
            "model": "anthropic/claude",
        });
        let rows = execution_meta_rows(Some(&meta), "native:directive");
        let arr = rows.as_array().unwrap();
        let by_label: std::collections::HashMap<&str, &str> = arr
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["value"].as_str().unwrap()))
            .collect();
        assert_eq!(by_label["capabilities minted"], "2");
        assert_eq!(by_label["trust class"], "trusted_bundle");
        assert_eq!(by_label["model"], "anthropic/claude");
        assert_eq!(by_label["runtime"], "native:directive");
        // Limits render as a compact key=value line.
        assert!(by_label["limits"].contains("turns=25"));
        assert!(by_label["limits"].contains("wall_secs=600"));
    }

    #[test]
    fn execution_meta_without_audit_record_is_runtime_only() {
        let rows = execution_meta_rows(None, "native:graph");
        let arr = rows.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["label"], serde_json::json!("runtime"));
        assert_eq!(arr[0]["value"], serde_json::json!("native:graph"));
    }

    #[test]
    fn execution_meta_drops_empty_runtime() {
        // A thread with neither an audit record nor an executor ref yields no rows
        // (the section reads empty rather than showing a blank runtime line).
        let rows = execution_meta_rows(None, "");
        assert!(rows.as_array().unwrap().is_empty());
    }

    #[test]
    fn execution_posture_coverage_preserves_failure_reason() {
        let available = ThreadMetaRead::Available(serde_json::json!({}));
        assert_eq!(available.coverage()["state"], "available");
        let missing = ThreadMetaRead::Unavailable("audit_record_missing");
        assert_eq!(missing.coverage()["state"], "unavailable");
        assert_eq!(missing.coverage()["reason"], "audit_record_missing");
        let corrupt = ThreadMetaRead::Unavailable("audit_record_corrupt");
        assert_eq!(corrupt.coverage()["reason"], "audit_record_corrupt");
    }

    #[test]
    fn thread_meta_requires_the_current_node_signature() {
        let directory = tempfile::tempdir().unwrap();
        let current =
            ryeos_node_runtime::identity::NodeIdentity::create(&directory.path().join("node.pem"))
                .unwrap();
        let predecessor = ryeos_node_runtime::identity::NodeIdentity::create(
            &directory.path().join("old-node.pem"),
        )
        .unwrap();
        let body = serde_json::json!({"thread_id":"T-signed"}).to_string();
        let signed = lillux::signature::sign_content(&body, predecessor.signing_key(), "#", None);
        assert_eq!(
            verify_thread_meta_document(&signed, &current),
            Err("audit_record_signature_invalid")
        );
        assert_eq!(
            verify_thread_meta_document(&body, &current),
            Err("audit_record_signature_missing")
        );
    }

    #[test]
    fn valid_same_node_audit_record_cannot_be_reused_for_another_thread() {
        let directory = tempfile::tempdir().unwrap();
        let node =
            ryeos_node_runtime::identity::NodeIdentity::create(&directory.path().join("node.pem"))
                .unwrap();
        let body = serde_json::json!({
            "thread_id": "T-original",
            "item_ref": "tool:original",
            "capabilities": ["ryeos.execute.tool.original"]
        })
        .to_string();
        let signed = lillux::signature::sign_content(&body, node.signing_key(), "#", None);
        let verified = verify_thread_meta_document(&signed, &node).unwrap();
        assert!(audit_record_matches_subject(
            &verified,
            "T-original",
            "tool:original"
        ));
        assert!(!audit_record_matches_subject(
            &verified,
            "T-other",
            "tool:original"
        ));
        assert!(!audit_record_matches_subject(
            &verified,
            "T-original",
            "tool:other"
        ));
    }

    #[test]
    fn compact_limits_handles_non_object() {
        assert_eq!(
            compact_limits(&serde_json::json!("unbounded")),
            "\"unbounded\""
        );
        assert_eq!(compact_limits(&serde_json::json!({})), "{}");
    }

    use ryeos_app::thread_lifecycle::{FollowFact, follow_display_state, follow_role};

    #[test]
    fn follow_rows_none_is_empty() {
        // A non-follow thread contributes no rows — the section reads empty.
        assert!(follow_rows(None).as_array().unwrap().is_empty());
    }

    #[test]
    fn follow_rows_suspended_parent_labels_lineage() {
        let f = FollowFact {
            role: follow_role::SUSPENDED_PARENT,
            display_state: follow_display_state::SUSPENDED,
            phase: Some("waiting".to_string()),
            follow_node: Some("n_follow".to_string()),
            child_thread_id: Some("T-child".to_string()),
            child_chain_root_id: Some("T-child".to_string()),
            // Child still running → no terminal status → the row is dropped.
            child_terminal_status: None,
            parent_successor_thread_id: Some("T-succ".to_string()),
            cohort: Some(ryeos_app::thread_lifecycle::FollowCohortProgress {
                done: 1,
                expected: 3,
            }),
        };
        let rows = follow_rows(Some(&f));
        let by_label: std::collections::HashMap<&str, &str> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["value"].as_str().unwrap()))
            .collect();
        assert_eq!(by_label["state"], "suspended");
        assert_eq!(by_label["phase"], "waiting");
        assert_eq!(by_label["follow node"], "n_follow");
        assert_eq!(by_label["child chain"], "T-child");
        assert_eq!(by_label["resume successor"], "T-succ");
        assert_eq!(by_label["cohort progress"], "1 done / 3 expected");
        // The still-running child contributes no terminal-status row.
        assert!(!by_label.contains_key("child status"));
    }

    #[test]
    fn follow_rows_durable_resume_successor_is_state_plus_successor() {
        // The waiter-cleared durable form carries only role/state + successor id;
        // every child-identity row drops rather than showing a blank.
        let f = FollowFact {
            role: follow_role::RESUME_SUCCESSOR,
            display_state: follow_display_state::RESUMED,
            phase: None,
            follow_node: None,
            child_thread_id: None,
            child_chain_root_id: None,
            child_terminal_status: None,
            parent_successor_thread_id: Some("T-succ".to_string()),
            cohort: None,
        };
        let rows = follow_rows(Some(&f));
        let arr = rows.as_array().unwrap();
        let by_label: std::collections::HashMap<&str, &str> = arr
            .iter()
            .map(|r| (r["label"].as_str().unwrap(), r["value"].as_str().unwrap()))
            .collect();
        assert_eq!(by_label["state"], "resumed");
        assert_eq!(by_label["resume successor"], "T-succ");
        assert_eq!(arr.len(), 2, "only state + successor survive: {arr:?}");
    }
}
