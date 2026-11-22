//! The timeline fold host: a chain's events folded into timeline items by a
//! signed definition, returned as the last `limit` top-level items.
//!
//! Two endpoints serve it under different authorisations: the core service
//! `service:threads/timeline` (thread ownership) and the UI service
//! `ui.ryeos.timeline.thread` (browser seat). Both authorise the caller, then
//! call [`timeline_response`], so they return one response shape and share one
//! [`TimelineHost`] cache held in the application state's extension bag.
//!
//! Each (chain, definition) keeps a resumable fold in the cache; a request
//! pushes only the events newer than the cached highest `chain_seq`.
//!
//! With `since_chain_seq` the response is an upsert for a client that already
//! holds the window as of that seq: `order` lists the window's item ids, and
//! `items` carries every window item whose last seq is greater than `since`
//! (all of them when earlier entries were re-derived after `since`). Clusters
//! and recaps re-form as events arrive, but their last seq grows whenever their
//! content does, so unchanged items are never resent. A client that lacks an id
//! in `order` (an older item slid into the window) refetches without `since`.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use ryeos_app::bounded_lru::BoundedLru;
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::config_loading::{ConfigLoadContext, ConfigSpec, ResolveMode};
use ryeos_state::queries::ThreadUsageTotals;
use ryeos_timeline::{Definition, Folder, TIMELINE_CONTRACT_VERSION, Timeline};

const TIMELINE_CACHE_MAX_ENTRIES: usize = 64;
const PRESENTATION_CACHE_MAX_ENTRIES: usize = 256;

/// `(chain_root_id, definition_digest)`.
type TimelineCacheKey = (String, String);

/// One chain's resumable fold. A request locks it, pushes the events newer
/// than `highest`, and reads the timeline.
struct ChainFold {
    folder: Folder,
    /// Highest `chain_seq` pushed into `folder`; zero before any event.
    highest: i64,
    /// The folder's timeline as of `highest`.
    timeline: Arc<Timeline>,
}

/// Chain folds, one per chain and definition.
type TimelineCache = BoundedLru<TimelineCacheKey, Arc<Mutex<ChainFold>>>;

/// A tool's `presentation` block by admitted-launch-capsule hash. A capsule is
/// content-addressed and immutable, so an entry never goes stale; `None`
/// records a capsule that carries no block.
type PresentationCache = BoundedLru<String, Option<Value>>;

/// The process-local caches behind every timeline endpoint. One instance lives
/// in the application state's extension bag ([`timeline_host`]), so the core
/// and UI endpoints fold each chain once and serve both from it.
pub struct TimelineHost {
    timelines: TimelineCache,
    presentations: PresentationCache,
}

impl TimelineHost {
    fn new() -> Self {
        Self {
            timelines: TimelineCache::with_capacity(TIMELINE_CACHE_MAX_ENTRIES),
            presentations: PresentationCache::with_capacity(PRESENTATION_CACHE_MAX_ENTRIES),
        }
    }
}

/// The application's single [`TimelineHost`], created on first use.
pub fn timeline_host(state: &AppState) -> Arc<TimelineHost> {
    state.extensions.get_or_insert_with(TimelineHost::new)
}

pub const DEFAULT_DEFINITION_REF: &str = "config:ryeos/ui/timeline/thread";
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;
/// The chain-replay page cap: the store refuses a larger page.
const REPLAY_PAGE_EVENTS: usize = 500;
/// A tool's presentation block larger than this (serialized) is ignored.
const MAX_PRESENTATION_BYTES: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequest {
    chain_root_id: String,
    #[serde(default)]
    limit: Option<Value>,
    #[serde(default)]
    definition: Option<String>,
    #[serde(default)]
    since_chain_seq: Option<Value>,
}

/// A validated timeline request, shared by every endpoint that serves the fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineParams {
    pub chain_root_id: String,
    pub limit: usize,
    pub since: Option<i64>,
    pub definition_ref: String,
}

impl TimelineParams {
    /// Parse and validate the wire parameters. Authorisation is the caller's
    /// job and happens after this, before [`timeline_response`].
    pub fn parse(params: Value) -> Result<Self, HandlerError> {
        let request: RawRequest = serde_json::from_value(params).map_err(|error| {
            HandlerError::BadRequest(format!("invalid timeline request: {error}"))
        })?;
        let chain_root_id = request.chain_root_id.trim().to_string();
        if chain_root_id.is_empty() {
            return Err(HandlerError::BadRequest(
                "chain_root_id is required".to_string(),
            ));
        }
        Ok(Self {
            chain_root_id,
            limit: parse_limit(request.limit.as_ref())?,
            since: parse_since(request.since_chain_seq.as_ref())?,
            definition_ref: request
                .definition
                .as_deref()
                .unwrap_or(DEFAULT_DEFINITION_REF)
                .to_string(),
        })
    }
}

/// The `chain_seq` a client's window is current to: a non-negative number or
/// numeric string.
fn parse_since(raw: Option<&Value>) -> Result<Option<i64>, HandlerError> {
    let invalid =
        || HandlerError::BadRequest("since_chain_seq must be a non-negative integer".into());
    let parsed = match raw {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(number)) => number.as_i64().ok_or_else(invalid)?,
        Some(Value::String(text)) => text.trim().parse::<i64>().map_err(|_| invalid())?,
        Some(_) => return Err(invalid()),
    };
    if parsed < 0 {
        return Err(invalid());
    }
    Ok(Some(parsed))
}

/// Top-level item count requested: absent is the default; a number or a
/// numeric string within `1..=MAX_LIMIT` is accepted.
fn parse_limit(raw: Option<&Value>) -> Result<usize, HandlerError> {
    let invalid =
        || HandlerError::BadRequest(format!("limit must be an integer from 1 to {MAX_LIMIT}"));
    let parsed = match raw {
        None | Some(Value::Null) => return Ok(DEFAULT_LIMIT),
        Some(Value::Number(number)) => number.as_u64().ok_or_else(invalid)?,
        Some(Value::String(text)) => text.trim().parse::<u64>().map_err(|_| invalid())?,
        Some(_) => return Err(invalid()),
    };
    match usize::try_from(parsed) {
        Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => Ok(limit),
        _ => Err(invalid()),
    }
}

/// The window is a suffix of the top-level items: how many precede it.
fn older_count(total: usize, limit: usize) -> usize {
    total.saturating_sub(limit)
}

/// The limit that would reveal more of the chain: one default page wider, up
/// to the cap. `None` when nothing is older than the window or the window is
/// already as wide as it may get.
fn next_limit(older: usize, limit: usize) -> Option<usize> {
    let wider = limit.saturating_add(DEFAULT_LIMIT).min(MAX_LIMIT);
    (older > 0 && wider > limit).then_some(wider)
}

/// The chain's header: the root thread's status and the chain-wide usage
/// totals. Computed on every request, never cached with the fold, so the
/// status stays current.
fn chain_summary(status: &str, totals: &ThreadUsageTotals) -> Value {
    json!({
        "status": status,
        "input_tokens": totals.input_tokens,
        "output_tokens": totals.output_tokens,
        "spend_usd": totals.spend_usd,
        "turns": totals.completed_turns,
    })
}

/// Resolve the definition config item from the installed bundles and parse it. The digest is the sha256
/// of the item's verified content, so a changed definition never reuses a
/// folded timeline.
fn load_definition(state: &AppState, definition_ref: &str) -> Result<(Definition, String)> {
    let canonical = CanonicalRef::parse(definition_ref)
        .map_err(|error| HandlerError::BadRequest(format!("invalid definition ref: {error}")))?;
    if canonical.kind != "config" || canonical.suffix.is_some() {
        return Err(HandlerError::BadRequest(
            "definition must be an unsuffixed config reference".to_string(),
        )
        .into());
    }
    let engine = &state.engine;
    let roots = engine.resolution_roots(
        None,
        ryeos_engine::contracts::LogicalProject::Absent(
            ryeos_engine::contracts::LogicalProjectAbsence::Projectless,
        ),
    );
    let load_context = ConfigLoadContext {
        roots: &roots,
        parsers: &engine.parser_dispatcher,
        kinds: &engine.kinds,
        trust_store: &engine.trust_store,
        project_authority: None,
    };
    let resolved = ryeos_engine::config_loading::resolve_config_spec(
        &ConfigSpec {
            path: format!("{}.yaml", canonical.bare_id),
            mode: ResolveMode::FirstMatch,
        },
        &load_context,
    )
    .map_err(|error| HandlerError::Internal(format!("resolve timeline definition: {error}")))?;
    let Some(layer) = resolved.layers.first() else {
        return Err(HandlerError::Conflict(format!(
            "timeline definition is not installed: {definition_ref}"
        ))
        .into());
    };
    let value = ryeos_engine::config_loading::load_and_verify_trusted_config_file(
        &layer.path,
        &load_context,
    )
    .map_err(|error| {
        HandlerError::Internal(format!("timeline definition is untrusted: {error}"))
    })?;
    let digest = lillux::sha256_hex(&serde_json::to_vec(&value)?);
    let definition = Definition::from_value(value).map_err(HandlerError::Internal)?;
    Ok((definition, digest))
}

/// One page of a chain's durable events in ascending `chain_seq`, and the
/// cursor of the next page when there is one.
struct EventPage {
    events: Vec<Value>,
    next: Option<i64>,
}

/// A chain's timeline at its highest `chain_seq`.
struct Folded {
    highest: i64,
    /// Set when a push re-derived earlier entries (see `Folder::rewritten_at`).
    rewritten_at: Option<i64>,
    timeline: Arc<Timeline>,
}

/// The `presentation` block of a tool call's child thread, as the fold's
/// subject for that thread.
///
/// The block is read from the composed value the child's admitted launch
/// capsule pinned, so a call shows the presentation of the tool that ran, not
/// of the tool as it is now. This is a lean read: the thread row gives the
/// capsule hash, one bounded CAS read gives the capsule, and only the block is
/// kept, cached by hash. A thread with no capsule (a service call, a call
/// refused before it ran), a capsule that is gone, or one with no block has no
/// subject, and its entry draws as the definition says.
fn presentation_subject(
    state: &AppState,
    cache: &PresentationCache,
    thread_id: &str,
) -> Option<Value> {
    let hash = state
        .state_store
        .admitted_launch_capsule_hash(thread_id)
        .ok()??;
    let block = match cache.get(&hash) {
        Some(block) => block,
        None => {
            let capsule = state
                .acquire_cas_read()
                .ok()?
                .cas()
                .get_object_bounded(
                    &hash,
                    ryeos_state::objects::MAX_ADMITTED_LAUNCH_CAPSULE_BYTES as u64,
                )
                .ok()??;
            let block = presentation_block(&capsule);
            cache.insert(hash, block.clone());
            block
        }
    };
    block.map(|block| json!({ "presentation": block }))
}

/// The presentation block inside a capsule's exact program: the composed value
/// of the tool as it ran sits one `composed` level inside the resolution
/// output's composed view.
fn presentation_block(capsule: &Value) -> Option<Value> {
    let block =
        capsule.pointer("/exact_program/resolution_output/composed/composed/presentation")?;
    let bounded = block.is_object()
        && serde_json::to_vec(block).is_ok_and(|bytes| bytes.len() <= MAX_PRESENTATION_BYTES);
    bounded.then(|| block.clone())
}

/// Fold of the chain up to its newest event. Only events after the cached
/// highest seq are fetched (500 at a time) and pushed; the first request for a
/// (chain, definition) folds from the start. Before a page is pushed,
/// `subject` is asked for each thread the definition lets present an entry in
/// it; a thread it has no subject for draws as the definition says.
fn timeline_for(
    cache: &TimelineCache,
    key: &TimelineCacheKey,
    definition: &Definition,
    mut fetch: impl FnMut(Option<i64>) -> Result<EventPage>,
    mut subject: impl FnMut(&str) -> Option<Value>,
) -> Result<Folded> {
    let entry = match cache.get(key) {
        Some(entry) => entry,
        None => {
            let entry = Arc::new(Mutex::new(ChainFold {
                folder: Folder::new(definition.clone()),
                highest: 0,
                timeline: Arc::new(Timeline {
                    items: Vec::new(),
                    undeclared: Default::default(),
                    notice_noun: None,
                    lane_matches: Vec::new(),
                }),
            }));
            cache.insert(key.clone(), entry.clone());
            entry
        }
    };
    let mut chain = entry
        .lock()
        .map_err(|_| HandlerError::Internal("timeline fold lock poisoned".into()))?;
    let mut grew = false;
    let fetched = loop {
        let after = (chain.highest > 0).then_some(chain.highest);
        let page = match fetch(after) {
            Ok(page) => page,
            Err(error) => break Err(error),
        };
        let Some(last) = page.events.last().and_then(|e| e["chain_seq"].as_i64()) else {
            break Ok(());
        };
        for thread in chain.folder.subjects_wanted(&page.events) {
            if let Some(found) = subject(&thread) {
                chain.folder.provide(thread, found);
            }
        }
        if let Err(error) = chain.folder.push(&page.events) {
            break Err(HandlerError::Internal(error).into());
        }
        chain.highest = last;
        grew = true;
        if page.next.is_none() {
            break Ok(());
        }
    };
    // Rebuild even when a later page failed: the folder already holds the
    // pages pushed before it, and the cached timeline must match the folder
    // or a later request with nothing new would serve the stale one.
    if grew {
        chain.timeline = Arc::new(chain.folder.timeline());
    }
    fetched?;
    Ok(Folded {
        highest: chain.highest,
        rewritten_at: chain.folder.rewritten_at(),
        timeline: chain.timeline.clone(),
    })
}

/// Whether the chain's root status says no more work is happening in the
/// chain. A `continued` root hands its work to a successor in the same chain,
/// so it is not settled.
fn chain_settled(status: &str) -> bool {
    status != "continued" && crate::routes::invokers::stream_helpers::is_terminal_status(status)
}

fn window_response(
    chain_root_id: &str,
    definition_ref: &str,
    definition_digest: &str,
    folded: &Folded,
    summary: Value,
    limit: usize,
    since: Option<i64>,
) -> Result<Value> {
    let timeline = &folded.timeline;
    // An activity a dead thread left running must not read as work in
    // progress, so a settled chain has no lane whatever its entries say.
    let status = summary
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let lane = if chain_settled(status) {
        None
    } else {
        timeline.lane(status)
    };
    let older = older_count(timeline.items.len(), limit);
    let window = &timeline.items[older..];
    let mut response = json!({
        "timeline_contract": TIMELINE_CONTRACT_VERSION,
        "chain_root_id": chain_root_id,
        "definition_ref": definition_ref,
        "definition_digest": definition_digest,
        "highest_chain_seq": folded.highest,
        "older": older,
        "next_limit": next_limit(older, limit),
        "summary": summary,
        "undeclared": serde_json::to_value(&timeline.undeclared)?,
        "notice_noun": serde_json::to_value(&timeline.notice_noun)?,
        "lane": serde_json::to_value(lane)?,
    });
    let Some(since) = since else {
        response["items"] = serde_json::to_value(window)?;
        return Ok(response);
    };
    let resend_all = folded.rewritten_at.is_some_and(|at| since < at);
    let changed: Vec<_> = window
        .iter()
        .filter(|item| resend_all || item.last_seq() > since)
        .collect();
    response["order"] = window.iter().map(|item| item.id()).collect();
    response["items"] = serde_json::to_value(changed)?;
    Ok(response)
}

/// The timeline response for an already-authorised caller: the chain's last
/// `params.limit` top-level items (or an upsert since `params.since`), the
/// chain summary and the definition's identity. `status` is the chain root
/// thread's current status, which the caller's authorisation read.
pub fn timeline_response(state: &AppState, params: &TimelineParams, status: &str) -> Result<Value> {
    let (definition, definition_digest) = load_definition(state, &params.definition_ref)?;
    let host = timeline_host(state);
    let chain_root_id = &params.chain_root_id;
    let key = (chain_root_id.clone(), definition_digest.clone());
    let folded = timeline_for(
        &host.timelines,
        &key,
        &definition,
        |after| {
            let page = state.events.replay(&EventReplayParams {
                thread_id: None,
                chain_root_id: Some(chain_root_id.clone()),
                after_chain_seq: after,
                limit: REPLAY_PAGE_EVENTS,
                event_types: None,
            })?;
            Ok(EventPage {
                events: page
                    .events
                    .iter()
                    .map(serde_json::to_value)
                    .collect::<Result<Vec<Value>, _>>()?,
                next: page.next_cursor,
            })
        },
        |thread| presentation_subject(state, &host.presentations, thread),
    )?;
    let totals = state
        .state_store
        .chain_usage_totals(chain_root_id)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    window_response(
        chain_root_id,
        &params.definition_ref,
        &definition_digest,
        &folded,
        chain_summary(status, &totals),
        params.limit,
        params.since,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_timeline::{Item, Recap, fold};
    use std::collections::{BTreeMap, HashMap};
    use std::path::PathBuf;

    fn timeline(items: usize) -> Timeline {
        let items = (0..items)
            .map(|index| {
                Item::Recap(Recap {
                    id: format!("item-{index}"),
                    exchange: "exchange".to_string(),
                    work: 0,
                    duration_s: None,
                    members: Vec::new(),
                    seq: index as i64,
                })
            })
            .collect();
        Timeline {
            items,
            undeclared: BTreeMap::new(),
            notice_noun: None,
            lane_matches: Vec::new(),
        }
    }

    fn folded(items: usize, highest: i64) -> Folded {
        Folded {
            highest,
            rewritten_at: None,
            timeline: Arc::new(timeline(items)),
        }
    }

    fn summary() -> Value {
        chain_summary("running", &ThreadUsageTotals::default())
    }

    #[test]
    fn the_response_carries_the_chain_summary_beside_the_window() {
        let totals = ThreadUsageTotals {
            thread_count: 2,
            completed_turns: 3,
            input_tokens: 1200,
            output_tokens: 340,
            spend_usd: 0.0421,
        };
        let summary = chain_summary("running", &totals);
        let value =
            window_response("c", "config:x", "d", &folded(1, 9), summary, 200, None).unwrap();
        assert_eq!(
            value["summary"],
            json!({
                "status": "running",
                "input_tokens": 1200,
                "output_tokens": 340,
                "spend_usd": 0.0421,
                "turns": 3,
            })
        );
    }

    fn working_by_entry() -> ryeos_timeline::LaneMatch {
        ryeos_timeline::LaneMatch {
            label: "Working".to_string(),
            entry: Some("act:1".to_string()),
            status: None,
        }
    }

    #[test]
    fn the_response_carries_the_lane_and_the_notice_noun_beside_the_window() {
        let mut state = folded(1, 9);
        let mut timeline = timeline(1);
        timeline.lane_matches = vec![working_by_entry()];
        timeline.notice_noun = Some(ryeos_timeline::Noun {
            key: "system_event".to_string(),
            one: "system event".to_string(),
            many: "system events".to_string(),
        });
        state.timeline = Arc::new(timeline);
        let value = window_response("c", "config:x", "d", &state, summary(), 200, None).unwrap();
        assert_eq!(value["lane"], json!({"label": "Working", "entry": "act:1"}));
        assert_eq!(
            value["notice_noun"],
            json!({"key": "system_event", "one": "system event", "many": "system events"})
        );
        let idle =
            window_response("c", "config:x", "d", &folded(1, 9), summary(), 200, None).unwrap();
        assert_eq!(idle["lane"], Value::Null);
    }

    #[test]
    fn a_settled_chain_has_no_lane_whatever_its_entries_say() {
        let mut state = folded(1, 9);
        let mut timeline = timeline(1);
        timeline.lane_matches = vec![working_by_entry()];
        state.timeline = Arc::new(timeline);
        let respond = |status: &str| {
            let summary = chain_summary(status, &ThreadUsageTotals::default());
            window_response("c", "config:x", "d", &state, summary, 200, None).unwrap()
        };
        for status in ["running", "created", "continued"] {
            assert_eq!(respond(status)["lane"]["label"], "Working", "{status}");
        }
        for status in ["killed", "timed_out", "failed", "cancelled", "completed"] {
            assert_eq!(respond(status)["lane"], Value::Null, "{status}");
        }
    }

    #[test]
    fn a_status_row_names_a_running_chain_with_no_entry_and_a_settled_one_has_none() {
        let mut state = folded(0, 9);
        let mut timeline = timeline(0);
        timeline.lane_matches = vec![ryeos_timeline::LaneMatch {
            label: "Working".to_string(),
            entry: None,
            status: Some("running".to_string()),
        }];
        state.timeline = Arc::new(timeline);
        let respond = |status: &str| {
            let summary = chain_summary(status, &ThreadUsageTotals::default());
            window_response("c", "config:x", "d", &state, summary, 200, None).unwrap()
        };
        assert_eq!(
            respond("running")["lane"],
            json!({"label": "Working", "entry": null})
        );
        assert_eq!(respond("created")["lane"], Value::Null);
        assert_eq!(respond("killed")["lane"], Value::Null);
    }

    #[test]
    fn next_limit_widens_by_a_page_until_the_cap_and_only_when_older_remains() {
        assert_eq!(next_limit(5, 200), Some(400));
        assert_eq!(next_limit(5, 1900), Some(MAX_LIMIT));
        assert_eq!(next_limit(5, MAX_LIMIT), None, "already at the cap");
        assert_eq!(next_limit(0, 200), None, "nothing older to reveal");
        let value =
            window_response("c", "config:x", "d", &folded(500, 9), summary(), 200, None).unwrap();
        assert_eq!(value["next_limit"], 400);
        let value =
            window_response("c", "config:x", "d", &folded(5, 9), summary(), 200, None).unwrap();
        assert!(value["next_limit"].is_null());
    }

    #[test]
    fn limit_defaults_and_accepts_numbers_and_strings_within_bounds() {
        assert_eq!(parse_limit(None).unwrap(), DEFAULT_LIMIT);
        assert_eq!(parse_limit(Some(&json!(1))).unwrap(), 1);
        assert_eq!(parse_limit(Some(&json!("2000"))).unwrap(), MAX_LIMIT);
        assert_eq!(parse_limit(Some(&json!(" 50 "))).unwrap(), 50);
    }

    #[test]
    fn limit_refuses_out_of_range_and_malformed_values() {
        for bad in [
            json!(0),
            json!(2001),
            json!(-1),
            json!(1.5),
            json!("abc"),
            json!(""),
            json!(true),
            json!([1]),
        ] {
            assert!(parse_limit(Some(&bad)).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn since_accepts_absent_numbers_and_strings_and_refuses_the_rest() {
        assert_eq!(parse_since(None).unwrap(), None);
        assert_eq!(parse_since(Some(&Value::Null)).unwrap(), None);
        assert_eq!(parse_since(Some(&json!(0))).unwrap(), Some(0));
        assert_eq!(parse_since(Some(&json!(" 42 "))).unwrap(), Some(42));
        for bad in [json!(-1), json!("x"), json!(1.5), json!(true)] {
            assert!(parse_since(Some(&bad)).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn window_is_the_suffix_and_older_counts_the_rest() {
        assert_eq!(older_count(10, 3), 7);
        assert_eq!(older_count(3, 3), 0);
        assert_eq!(older_count(2, 200), 0);
        let value =
            window_response("c", "config:x", "d", &folded(5, 9), summary(), 2, None).unwrap();
        assert_eq!(value["older"], 3);
        assert_eq!(value["highest_chain_seq"], 9);
        assert!(value.get("order").is_none());
        let items = value["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], "item-3");
        assert_eq!(items[1]["id"], "item-4");
    }

    #[test]
    fn since_sends_the_order_and_only_items_that_changed() {
        let value =
            window_response("c", "config:x", "d", &folded(5, 9), summary(), 4, Some(2)).unwrap();
        assert_eq!(
            value["order"],
            json!(["item-1", "item-2", "item-3", "item-4"])
        );
        let ids: Vec<&str> = value["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["item-3", "item-4"]);
    }

    #[test]
    fn since_before_a_rewrite_resends_the_whole_window() {
        let mut state = folded(5, 9);
        state.rewritten_at = Some(3);
        let value = window_response("c", "config:x", "d", &state, summary(), 4, Some(1)).unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 4);
        let value = window_response("c", "config:x", "d", &state, summary(), 4, Some(3)).unwrap();
        assert_eq!(value["items"].as_array().unwrap().len(), 1);
    }

    fn definition() -> Definition {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let text = std::fs::read_to_string(
            root.join("bundles/ryeos-ui/.ai/config/ryeos/ui/timeline/thread.yaml"),
        )
        .unwrap();
        Definition::from_yaml(&text).unwrap()
    }

    fn fixture(name: &str) -> Vec<Value> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../engine/ryeos-timeline/tests/fixtures")
            .join(format!("{name}.json"));
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        doc["events"].as_array().unwrap().clone()
    }

    const FIXTURES: [&str; 7] = [
        "tool_run_completed",
        "tool_run_failed",
        "directive_provider_failure",
        "directive_tool_then_handoff_failure",
        "graph_foreach_failure_recovery",
        "directive_continuation_with_successor",
        "directive_turn_record",
    ];

    /// A store holding the first `count` events, served in pages of three.
    fn store(events: &[Value], count: usize) -> impl FnMut(Option<i64>) -> Result<EventPage> + '_ {
        move |after| {
            let rest: Vec<&Value> = events[..count]
                .iter()
                .filter(|e| after.is_none_or(|a| e["chain_seq"].as_i64().unwrap() > a))
                .collect();
            let page: Vec<Value> = rest.iter().take(3).map(|e| (*e).clone()).collect();
            let next = (rest.len() > 3)
                .then(|| page.last().and_then(|e| e["chain_seq"].as_i64()))
                .flatten();
            Ok(EventPage { events: page, next })
        }
    }

    /// The window as a client holds it: items by id, in order.
    fn merge(previous: &Value, update: &Value) -> Option<Vec<Value>> {
        let mut by_id: HashMap<String, Value> = HashMap::new();
        for item in previous["items"].as_array().unwrap() {
            by_id.insert(item["id"].as_str().unwrap().to_string(), item.clone());
        }
        for item in update["items"].as_array().unwrap() {
            by_id.insert(item["id"].as_str().unwrap().to_string(), item.clone());
        }
        update["order"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| by_id.get(id.as_str().unwrap()).cloned())
            .collect()
    }

    #[test]
    fn the_cached_fold_equals_a_fresh_fold_as_the_chain_grows() {
        let def = definition();
        for name in FIXTURES {
            let events = fixture(name);
            let cache = TimelineCache::with_capacity(4);
            let key = ("chain".to_string(), "digest".to_string());
            for count in 1..=events.len() {
                let cached =
                    timeline_for(&cache, &key, &def, store(&events, count), |_| None).unwrap();
                let fresh = fold(&events[..count], &def).unwrap();
                assert_eq!(*cached.timeline, fresh, "{name} at {count}");
                assert_eq!(
                    cached.highest,
                    events[count - 1]["chain_seq"].as_i64().unwrap()
                );
            }
            let again =
                timeline_for(&cache, &key, &def, store(&events, events.len()), |_| None).unwrap();
            assert_eq!(*again.timeline, fold(&events, &def).unwrap());
        }
    }

    #[test]
    fn a_since_response_merged_into_the_previous_window_equals_the_full_window() {
        let def = definition();
        for name in FIXTURES {
            let events = fixture(name);
            let cache = TimelineCache::with_capacity(4);
            let key = ("chain".to_string(), "digest".to_string());
            let mut held: Option<Value> = None;
            for count in 1..=events.len() {
                let now =
                    timeline_for(&cache, &key, &def, store(&events, count), |_| None).unwrap();
                let full = window_response("c", "r", "d", &now, summary(), 200, None).unwrap();
                if let Some(previous) = &held {
                    let since = previous["highest_chain_seq"].as_i64().unwrap();
                    let update =
                        window_response("c", "r", "d", &now, summary(), 200, Some(since)).unwrap();
                    let merged = merge(previous, &update)
                        .unwrap_or_else(|| panic!("{name} at {count}: an id was missing"));
                    assert_eq!(Value::Array(merged), full["items"], "{name} at {count}");
                }
                held = Some(full);
            }
        }
    }
    const FETCH_OP: &str = "0000000000000000000000000000000000000000000000000000000000000007";

    /// A directive turn that calls a tool whose own thread is `T-2`.
    fn fetch_chain() -> Vec<Value> {
        let ev = |seq: i64, thread: &str, event_type: &str, payload: Value| {
            json!({"chain_seq": seq, "chain_root_id": "T-1", "thread_id": thread,
                   "ts": format!("2026-11-16T00:00:{seq:02}Z"),
                   "event_type": event_type, "payload": payload})
        };
        vec![
            ev(
                1,
                "T-1",
                "cognition_in",
                json!({"origin": "launch", "parts": [{"type": "text", "text": "go"}]}),
            ),
            ev(
                2,
                "T-1",
                "cognition_out",
                json!({"finish": "tool_calls", "turn": 1, "parts": [
                    {"type": "tool_call", "call_id": "c1", "name": "web_fetch",
                     "arguments": {"url": "https://example.com/a"}}]}),
            ),
            ev(
                3,
                "T-1",
                "tool_call_start",
                json!({"operation_id": FETCH_OP, "call_id": "c1", "tool": "web_fetch"}),
            ),
            ev(
                4,
                "T-1",
                "child_thread_spawned",
                json!({"child_thread_id": "T-2", "item_id": "tool:rye/web/fetch",
                       "operation_id": FETCH_OP}),
            ),
            ev(
                5,
                "T-1",
                "tool_call_result",
                json!({"operation_id": FETCH_OP, "call_id": "c1", "tool": "web_fetch",
                       "status": "ok", "guard": {"result_size_bytes": 9, "truncated": false},
                       "parts": [{"type": "text", "text": "{\"output\":\"page text\"}"}]}),
            ),
            ev(
                6,
                "T-1",
                "cognition_out",
                json!({"finish": "stop", "turn": 2, "parts": [{"type": "text", "text": "done"}]}),
            ),
        ]
    }

    fn fetch_subject() -> Value {
        json!({"presentation": {
            "counts_as": "fetch",
            "headline": ["arguments.url"],
            "body": ["result.output"],
        }})
    }

    fn fresh_with_subject(events: &[Value], def: &Definition) -> Timeline {
        let mut folder = Folder::new(def.clone());
        for thread in folder.subjects_wanted(events) {
            folder.provide(thread, fetch_subject());
        }
        folder.push(events).unwrap();
        folder.timeline()
    }

    fn resolver(thread: &str) -> Option<Value> {
        (thread == "T-2").then(fetch_subject)
    }

    #[test]
    fn a_presented_chain_folds_the_same_cached_as_fresh_at_every_length() {
        let def = definition();
        let events = fetch_chain();
        let cache = TimelineCache::with_capacity(4);
        let key = ("chain".to_string(), "digest".to_string());
        for count in 1..=events.len() {
            let cached = timeline_for(&cache, &key, &def, store(&events, count), resolver).unwrap();
            assert_eq!(
                *cached.timeline,
                fresh_with_subject(&events[..count], &def),
                "at {count}"
            );
        }
        let text = serde_json::to_string(
            &*timeline_for(&cache, &key, &def, store(&events, 6), resolver)
                .unwrap()
                .timeline,
        )
        .unwrap();
        assert!(text.contains("https://example.com/a"));
        assert!(text.contains("page text"));
    }

    #[test]
    fn a_since_response_stays_whole_when_presentation_arrives() {
        let def = definition();
        let events = fetch_chain();
        let cache = TimelineCache::with_capacity(4);
        let key = ("chain".to_string(), "digest".to_string());
        let mut held: Option<Value> = None;
        for count in 1..=events.len() {
            let now = timeline_for(&cache, &key, &def, store(&events, count), resolver).unwrap();
            let full = window_response("c", "r", "d", &now, summary(), 200, None).unwrap();
            if let Some(previous) = &held {
                let since = previous["highest_chain_seq"].as_i64().unwrap();
                let update =
                    window_response("c", "r", "d", &now, summary(), 200, Some(since)).unwrap();
                let merged = merge(previous, &update)
                    .unwrap_or_else(|| panic!("at {count}: an id was missing"));
                assert_eq!(Value::Array(merged), full["items"], "at {count}");
            }
            held = Some(full);
        }
    }

    #[test]
    fn a_thread_without_a_subject_draws_as_the_definition_says() {
        let def = definition();
        let events = fetch_chain();
        let cache = TimelineCache::with_capacity(4);
        let key = ("chain".to_string(), "digest".to_string());
        let cached = timeline_for(&cache, &key, &def, store(&events, 6), |_| None).unwrap();
        assert_eq!(*cached.timeline, fold(&events, &def).unwrap());
    }

    #[test]
    fn the_block_is_read_from_the_capsules_composed_tool_value() {
        let capsule = json!({"exact_program": {"resolution_output": {"composed": {
            "composed": {"presentation": {"headline": ["arguments.url"]}}}}}});
        assert_eq!(
            presentation_block(&capsule),
            Some(json!({"headline": ["arguments.url"]}))
        );
        // No block, a block that is not a mapping, or one over the byte bound.
        assert_eq!(presentation_block(&json!({"exact_program": {}})), None);
        let wrong = json!({"exact_program": {"resolution_output": {"composed": {
            "composed": {"presentation": "x"}}}}});
        assert_eq!(presentation_block(&wrong), None);
        let big = json!({"exact_program": {"resolution_output": {"composed": {
            "composed": {"presentation": {"body": ["a".repeat(MAX_PRESENTATION_BYTES)]}}}}}});
        assert_eq!(presentation_block(&big), None);
    }

    #[test]
    fn the_response_names_the_contract_version() {
        let value =
            window_response("c", "config:x", "d", &folded(1, 9), summary(), 200, None).unwrap();
        assert_eq!(value["timeline_contract"], TIMELINE_CONTRACT_VERSION);
    }

    /// The field names the contract document lists for the response, by when
    /// they are present: `always`, or `upsert` (only when `since_chain_seq`
    /// was given).
    fn documented_response_fields() -> (Vec<String>, Vec<String>) {
        let text = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../bundles/ryeos-ui/.ai/knowledge/ryeos/ui/timeline-v1.md"),
        )
        .expect("the timeline contract document is part of the repository");
        let mut always = Vec::new();
        let mut upsert = Vec::new();
        let mut in_table = false;
        for line in text.lines() {
            if let Some(heading) = line.strip_prefix("## ") {
                in_table = heading.trim() == "Response fields";
                continue;
            }
            if !in_table {
                continue;
            }
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            if cells.len() < 4 || !cells[1].starts_with('`') {
                continue;
            }
            let name = cells[1].trim_matches('`').to_string();
            match cells[2] {
                "always" => always.push(name),
                "upsert" => upsert.push(name),
                other => panic!("unknown presence {other:?} for response field {name}"),
            }
        }
        (always, upsert)
    }

    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn the_documented_response_fields_are_the_fields_the_host_returns() {
        let (always, upsert) = documented_response_fields();
        let mut whole = always.clone();
        whole.sort();
        let mut with_since: Vec<String> = always.iter().chain(&upsert).cloned().collect();
        with_since.sort();
        let state = folded(3, 9);
        let full = window_response("c", "config:x", "d", &state, summary(), 200, None).unwrap();
        assert_eq!(sorted_keys(&full), whole, "whole-window response");
        let update =
            window_response("c", "config:x", "d", &state, summary(), 200, Some(1)).unwrap();
        assert_eq!(sorted_keys(&update), with_since, "upsert response");
    }
}
