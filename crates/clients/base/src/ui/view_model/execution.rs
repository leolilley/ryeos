use super::super::event::RyeOsUiIntent;
use super::{bound_view_vm, RyeOsCore, RyeOsTone, RyeOsViewVm};

// Keep the established `ui::view_model::RyeOsTimelineEntryVm` path stable
// while grouping execution presentation behind this module.
pub use super::super::timeline::RyeOsTimelineEntryVm;

/// The deep-watch header for a braid: one summary line built from the source's
/// `summary` (chain status + chain-wide usage totals, from chain_replay).
/// Returns `None` when the source carries no `summary` — any non-chain timeline —
/// so the header only appears where it means something.
pub(crate) fn timeline_summary_entry(response: &serde_json::Value) -> Option<RyeOsTimelineEntryVm> {
    let summary = response.get("summary")?;
    let status = summary.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let input = summary
        .get("input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output = summary
        .get("output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cost = summary
        .get("spend_usd")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let turns = summary.get("turns").and_then(|v| v.as_i64()).unwrap_or(0);
    let primary = format!("{status} · ↑{input} ↓{output} · ${cost:.4} · {turns} turns");
    Some(RyeOsTimelineEntryVm::Line {
        primary,
        meta: None,
        tone: status_tone(status),
        intent: None,
        secondary_intent: None,
    })
}

/// The response a facet-backed view renders: the seat-fold value at `facet`,
/// resolved through the shared `@facet:` grammar (so a dotted path like
/// `selection.summary` reads the field within the `selection` facet). `None`
/// when the facet is unset — the view then falls back to its `source` fetch.
pub(super) fn facet_backed_response(
    core: &RyeOsCore,
    facet: &str,
) -> Option<serde_json::Value> {
    let fold = core.seat.fold();
    let resolved = super::super::content::resolve_params(
        &serde_json::Value::String(format!("@facet:{facet}")),
        |key| fold.get(key).cloned(),
    );
    (!resolved.is_null()).then_some(resolved)
}

/// Map a thread/chain status to a tone (the same status→tone vocabulary the
/// list/detail tone blocks declare, in code here for the summary header). Matches
/// the typed [`ThreadStatus`] variants so a new status is a compile error here,
/// not a silently-neutral string.
pub(super) fn status_tone(status: &str) -> RyeOsTone {
    use super::super::dto::ThreadStatus;
    match ThreadStatus::from_wire(status) {
        ThreadStatus::Running | ThreadStatus::Created => RyeOsTone::Accent,
        ThreadStatus::Failed | ThreadStatus::Killed | ThreadStatus::TimedOut => RyeOsTone::Danger,
        ThreadStatus::Cancelled => RyeOsTone::Warn,
        ThreadStatus::Completed | ThreadStatus::Continued => RyeOsTone::Good,
        ThreadStatus::Unknown => RyeOsTone::Neutral,
    }
}

/// The timeline entry under the point in the focused feed lens, if the focused
/// view is a timeline with a point on an entry. The single home for reading the
/// focused feed entry — both the Enter intent and command-overlay secondary
/// intents derive from it.
pub(super) fn focused_timeline_entry(core: &RyeOsCore) -> Option<RyeOsTimelineEntryVm> {
    let tile_id = core.workspace.focused_tile;
    let view = core.workspace.focused_view()?;
    if let RyeOsViewVm::Timeline {
        entries, selected, ..
    } = bound_view_vm(core, tile_id, &view.view_ref)
    {
        return selected.and_then(|i| entries.into_iter().nth(i));
    }
    None
}

/// The focused feed entry's secondary affordance — the retry a recoverable
/// failed terminal carries. Surfaced through the commands overlay (its Shift+Enter
/// secondary and a distinct "Retry failed turn" item), never a direct feed key,
/// so Enter stays inspect.
pub(super) fn retry_intent_for_focused_row(core: &RyeOsCore) -> Option<RyeOsUiIntent> {
    match focused_timeline_entry(core)? {
        RyeOsTimelineEntryVm::Line {
            secondary_intent, ..
        } => secondary_intent,
        _ => None,
    }
}
