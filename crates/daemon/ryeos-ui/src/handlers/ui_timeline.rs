//! `ui.ryeos.timeline.thread` — a chain's events folded into timeline items by
//! a signed definition, returned as the last `limit` top-level items.
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

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::event_store_service::EventReplayParams;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::config_loading::{ConfigLoadContext, ConfigSpec, ResolveMode};
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::queries::ThreadUsageTotals;
use ryeos_timeline::{Definition, Folder, Timeline};

use crate::state::{ChainFold, TimelineCache, TimelineCacheKey};

const SERVICE_REF: &str = "service:ui/ryeos-ui/timeline/thread";
const ENDPOINT: &str = "ui.ryeos.timeline.thread";
const DEFAULT_DEFINITION_REF: &str = "config:ryeos/ui/timeline/thread";
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 2_000;
/// The chain-replay page cap: the store refuses a larger page.
const REPLAY_PAGE_EVENTS: usize = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineRequest {
    chain_root_id: String,
    #[serde(default)]
    limit: Option<Value>,
    #[serde(default)]
    definition: Option<String>,
    #[serde(default)]
    since_chain_seq: Option<Value>,
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

/// Resolve the definition config item and parse it. The digest is the sha256
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
    let roots = engine.resolution_roots(None);
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
        return Err(HandlerError::NotFound.into());
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

/// Fold of the chain up to its newest event. Only events after the cached
/// highest seq are fetched (500 at a time) and pushed; the first request for a
/// (chain, definition) folds from the start.
fn timeline_for(
    cache: &TimelineCache,
    key: &TimelineCacheKey,
    definition: &Definition,
    mut fetch: impl FnMut(Option<i64>) -> Result<EventPage>,
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
    let older = older_count(timeline.items.len(), limit);
    let window = &timeline.items[older..];
    let mut response = json!({
        "chain_root_id": chain_root_id,
        "definition_ref": definition_ref,
        "definition_digest": definition_digest,
        "highest_chain_seq": folded.highest,
        "older": older,
        "next_limit": next_limit(older, limit),
        "summary": summary,
        "undeclared": serde_json::to_value(&timeline.undeclared)?,
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

pub async fn handle(params: Value, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let request: TimelineRequest = serde_json::from_value(params)
        .map_err(|error| HandlerError::BadRequest(format!("invalid timeline request: {error}")))?;
    let chain_root_id = request.chain_root_id.trim().to_string();
    if chain_root_id.is_empty() {
        return Err(HandlerError::BadRequest("chain_root_id is required".to_string()).into());
    }
    let limit = parse_limit(request.limit.as_ref())?;
    let since = parse_since(request.since_chain_seq.as_ref())?;
    let definition_ref = request
        .definition
        .as_deref()
        .unwrap_or(DEFAULT_DEFINITION_REF)
        .to_string();

    // The caller must own the chain's root thread, as `events.chain_replay`
    // requires, judged by the exact-thread subject authority UI reads share.
    let subject = crate::thread_authorization::authorize_exact_thread_subjects(
        &ctx,
        &state,
        &caller,
        &[&chain_root_id],
    )?
    .pop()
    .expect("one exact thread authorization returns one signed subject");
    if subject.chain_root_id != chain_root_id {
        return Err(HandlerError::NotFound.into());
    }

    let (definition, definition_digest) = load_definition(&state, &definition_ref)?;
    let ui = crate::state::get_ui_state(&state)
        .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?;
    let key = (chain_root_id.clone(), definition_digest.clone());
    let folded = timeline_for(ui.timeline_cache(), &key, &definition, |after| {
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
    })?;
    let totals = state
        .state_store
        .chain_usage_totals(&chain_root_id)
        .map_err(|error| HandlerError::Internal(format!("{error:#}")))?;
    window_response(
        &chain_root_id,
        &definition_ref,
        &definition_digest,
        &folded,
        chain_summary(subject.status.as_str(), &totals),
        limit,
        since,
    )
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: SERVICE_REF,
    endpoint: ENDPOINT,
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| Box::pin(async move { handle(params, ctx, state).await }),
};

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

    const FIXTURES: [&str; 6] = [
        "tool_run_completed",
        "tool_run_failed",
        "directive_provider_failure",
        "directive_tool_then_handoff_failure",
        "graph_foreach_failure_recovery",
        "directive_continuation_with_successor",
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
                let cached = timeline_for(&cache, &key, &def, store(&events, count)).unwrap();
                let fresh = fold(&events[..count], &def).unwrap();
                assert_eq!(*cached.timeline, fresh, "{name} at {count}");
                assert_eq!(
                    cached.highest,
                    events[count - 1]["chain_seq"].as_i64().unwrap()
                );
            }
            let again = timeline_for(&cache, &key, &def, store(&events, events.len())).unwrap();
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
                let now = timeline_for(&cache, &key, &def, store(&events, count)).unwrap();
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
}
