//! `ui.ryeos.node.activity` — bounded activity visible to the exact caller.
//! Discovery is a projection; subjects and returned events are authenticated.
//! Counts describe observed subjects, never node-wide or exhaustive totals.
//!
//! The response is projectable data only — labeled `{label, value}` pulse
//! rows and raw event records. Which event kinds matter and how they read
//! is the consuming view's business (`projections.event_kinds`), never
//! this handler's.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

fn default_event_limit() -> usize {
    80
}

const MAX_EVENT_LIMIT: usize = 500;
const MAX_ACTIVITY_THREADS: usize = 128;
const MAX_THREAD_EVENTS: usize = 64;
const MAX_THREAD_EVENT_BYTES: usize = 128 * 1024;
const MAX_ACTIVITY_EVENT_BYTES: usize = 2 * 1024 * 1024;

fn default_window_hours() -> u64 {
    24
}

const MAX_WINDOW_HOURS: u64 = 24 * 30;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityRequest {
    #[serde(default = "default_event_limit")]
    pub event_limit: usize,
    /// Event kinds the caller treats as noise (content-declared — e.g. a
    /// view excluding seat facet writes from the feed).
    #[serde(default)]
    pub exclude_types: Vec<String>,
    /// The settled-work window for the pulse counts and usage totals.
    #[serde(default = "default_window_hours")]
    pub window_hours: u64,
}

pub async fn handle_activity(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;

    let req: ActivityRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;
    let window_hours = req.window_hours.clamp(1, MAX_WINDOW_HOURS);

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let since_iso = lillux::time::iso8601_from_unix_secs(
        now_secs.saturating_sub(Duration::from_secs(window_hours * 3600).as_secs()),
    );

    let project_root = caller.project_query_identity()?;
    let filter = ryeos_app::thread_lifecycle::ThreadListFilter {
        principal: project_root
            .is_none()
            .then(|| caller.principal_id().to_string()),
        project_root,
        ..Default::default()
    };
    let sort = ryeos_app::thread_lifecycle::ThreadSort::Watch;
    let mut subjects = BTreeMap::new();
    let (observed, scanned) = super::ui_threads::collect_authorized_thread_items(
        MAX_ACTIVITY_THREADS,
        sort,
        |limit, cursor| {
            state
                .state_store
                .list_threads_candidate_page(limit, &filter, sort, cursor)
        },
        |candidate| {
            let signed = match crate::thread_authorization::authorize_exact_thread_subjects(
                &ctx,
                &state,
                &caller,
                &[&candidate.thread_id],
            ) {
                Ok(mut subjects) => subjects.pop().expect("one requested subject"),
                Err(HandlerError::NotFound) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            if !super::ui_threads::listed_thread_matches_signed_subject(candidate, &signed) {
                return Err(HandlerError::NotFound.into());
            }
            subjects.insert(signed.thread_id.clone(), signed);
            Ok(true)
        },
    )?;
    let mut counts = BTreeMap::<String, usize>::new();
    for signed in subjects.values() {
        let status = signed.status.as_str();
        if matches!(status, "running" | "created")
            || signed
                .finished_at
                .as_deref()
                .is_some_and(|finished| finished >= since_iso.as_str())
        {
            *counts.entry(status.to_string()).or_default() += 1;
        }
    }
    let count_of = |status: &str| counts.get(status).copied().unwrap_or(0);

    let window = if window_hours.is_multiple_of(24) {
        format!("{}d", window_hours / 24)
    } else {
        format!("{window_hours}h")
    };
    // Pulse rows in the same projectable `{label, value}` shape the thread
    // inspect summary uses: one execution fact per row.
    let running = count_of("running") + count_of("created");
    let pulse = serde_json::json!([
        { "label": "observed running", "value": running.to_string() },
        { "label": format!("observed completed · {window}"), "value": count_of("completed").to_string() },
        { "label": format!("observed failed · {window}"), "value": count_of("failed").to_string() },
        { "label": format!("observed cancelled · {window}"), "value": count_of("cancelled").to_string() },
    ]);

    let event_limit = req.event_limit.clamp(1, MAX_EVENT_LIMIT);
    let mut events = Vec::new();
    let mut bytes_read = 0;
    let mut partial = observed.len() == MAX_ACTIVITY_THREADS
        || scanned == super::ui_threads::MAX_THREAD_LIST_CANDIDATES_SCANNED;
    let mut failed = false;
    for signed in subjects.values() {
        let remaining = MAX_ACTIVITY_EVENT_BYTES - bytes_read;
        if remaining == 0 {
            partial = true;
            break;
        }
        let read = (|| -> Result<_> {
            let page = state.state_store.recent_thread_events(
                &signed.chain_root_id,
                &signed.thread_id,
                None,
                MAX_THREAD_EVENTS,
                MAX_THREAD_EVENT_BYTES.min(remaining),
            )?;
            require_event_subject(&page.events, &signed.chain_root_id, &signed.thread_id)?;
            let events = state
                .state_store
                .authenticate_execution_event_candidates(&signed.chain_root_id, &page.events)?;
            Ok((events, page.has_more))
        })();
        let (verified, has_more) = match read {
            Ok(page) => page,
            Err(_) => {
                failed = true;
                break;
            }
        };
        partial |= has_more;
        for event in verified {
            let bytes = serde_json::to_vec(&event)?.len();
            if !charge_event_bytes(&mut bytes_read, bytes) {
                partial = true;
                break;
            }
            if event.ts >= since_iso && !req.exclude_types.contains(&event.event_type) {
                events.push(event);
            }
        }
    }
    let (events, events_coverage) = finish_event_observation(events, failed, partial, event_limit);

    Ok(serde_json::json!({
        "schema_version": "ryeos-ui.node.activity.v1",
        "pulse": pulse,
        "events": events,
        "coverage": {"state":"observed", "reason":"authorized_projection_subset_not_exhaustive"},
        "pulse_coverage": {"state":"observed", "reason":"authorized_subject_subset_not_totals", "observed_threads":observed.len(), "thread_limit":MAX_ACTIVITY_THREADS},
        "events_coverage": events_coverage,
        "usage_coverage": {"state":"unavailable", "reason":"signed_scoped_usage_not_available"},
    }))
}

fn finish_event_observation(
    mut events: Vec<ryeos_app::state_store::PersistedEventRecord>,
    failed: bool,
    mut partial: bool,
    event_limit: usize,
) -> (Vec<ryeos_app::state_store::PersistedEventRecord>, Value) {
    // Failure of any subject's signed read invalidates this event observation,
    // not the independently authenticated subject/pulse observation above.
    if failed {
        events.clear();
    }
    events.sort_by(|left, right| {
        (&right.ts, &right.thread_id, right.thread_seq).cmp(&(
            &left.ts,
            &left.thread_id,
            left.thread_seq,
        ))
    });
    partial |= events.len() > event_limit;
    events.truncate(event_limit);
    (events, event_coverage(failed, partial))
}

fn require_event_subject(
    events: &[ryeos_app::state_store::PersistedEventRecord],
    chain_root_id: &str,
    thread_id: &str,
) -> Result<()> {
    if events
        .iter()
        .any(|event| event.thread_id != thread_id || event.chain_root_id != chain_root_id)
    {
        anyhow::bail!("activity event locator changed authorized subject");
    }
    Ok(())
}

fn charge_event_bytes(used: &mut usize, bytes: usize) -> bool {
    let Some(next) = used
        .checked_add(bytes)
        .filter(|next| *next <= MAX_ACTIVITY_EVENT_BYTES)
    else {
        return false;
    };
    *used = next;
    true
}

fn event_coverage(failed: bool, partial: bool) -> Value {
    serde_json::json!({
        "state": if failed { "failed" } else if partial { "partial" } else { "observed" },
        "reason": if failed { "signed_event_read_failed" } else { "authorized_bounded_events_not_exhaustive" },
    })
}

pub const ACTIVITY_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/node/activity",
    endpoint: "ui.ryeos.node.activity",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_activity(params, ctx, state).await })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn event(thread_id: &str, thread_seq: i64) -> ryeos_app::state_store::PersistedEventRecord {
        ryeos_app::state_store::PersistedEventRecord {
            event_id: thread_seq,
            event_hash: Some("a".repeat(64)),
            chain_root_id: thread_id.into(),
            chain_seq: thread_seq,
            thread_id: thread_id.into(),
            thread_seq,
            event_type: "thread_created".into(),
            storage_class: "durable".into(),
            ts: "2026-10-08T00:00:00Z".into(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn failed_event_observation_discards_an_already_accumulated_page() {
        let (events, coverage) =
            finish_event_observation(vec![event("T-visible", 1)], true, false, 80);
        assert!(events.is_empty());
        assert_eq!(coverage["state"], "failed");
        assert_eq!(coverage["reason"], "signed_event_read_failed");
    }

    #[test]
    fn event_observation_bounds_preserve_deterministic_order_and_partial_coverage() {
        let (events, coverage) = finish_event_observation(
            vec![event("T-a", 1), event("T-b", 1), event("T-b", 2)],
            false,
            false,
            2,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(
            (events[0].thread_id.as_str(), events[0].thread_seq),
            ("T-b", 2)
        );
        assert_eq!(
            (events[1].thread_id.as_str(), events[1].thread_seq),
            ("T-b", 1)
        );
        assert_eq!(coverage["state"], "partial");
        let (_, coverage) = finish_event_observation(Vec::new(), false, true, 80);
        assert_eq!(coverage["state"], "partial");
    }

    #[test]
    fn activity_event_coverage_never_certifies_exhaustive_absence() {
        assert_eq!(event_coverage(false, false)["state"], "observed");
        assert_eq!(event_coverage(false, true)["state"], "partial");
        assert_eq!(event_coverage(true, true)["state"], "failed");
        assert_eq!(
            event_coverage(true, false)["reason"],
            "signed_event_read_failed"
        );
    }

    #[test]
    fn aggregate_event_budget_cannot_overflow_or_charge_failed_reservation() {
        let mut used = 0;
        assert!(charge_event_bytes(&mut used, MAX_ACTIVITY_EVENT_BYTES));
        assert!(!charge_event_bytes(&mut used, 1));
        assert_eq!(used, MAX_ACTIVITY_EVENT_BYTES);
        assert!(!charge_event_bytes(&mut used, usize::MAX));
        assert_eq!(used, MAX_ACTIVITY_EVENT_BYTES);
    }

    #[test]
    fn event_locator_cannot_substitute_another_subject_before_authentication() {
        let mut event = ryeos_app::state_store::PersistedEventRecord {
            event_id: 1,
            event_hash: Some("a".repeat(64)),
            chain_root_id: "root".into(),
            chain_seq: 1,
            thread_id: "authorized".into(),
            thread_seq: 1,
            event_type: "thread_created".into(),
            storage_class: "durable".into(),
            ts: "2026-10-08T00:00:00Z".into(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: serde_json::json!({}),
        };
        assert!(require_event_subject(std::slice::from_ref(&event), "root", "authorized").is_ok());
        event.thread_id = "foreign".into();
        assert!(require_event_subject(std::slice::from_ref(&event), "root", "authorized").is_err());
        event.thread_id = "authorized".into();
        event.chain_root_id = "foreign".into();
        assert!(require_event_subject(std::slice::from_ref(&event), "root", "authorized").is_err());
    }
}
