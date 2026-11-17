//! `ui.ryeos.timeline.thread` — a chain's events folded into timeline items by
//! a signed definition, returned as the last `limit` top-level items.

use std::sync::Arc;

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
use ryeos_timeline::{Definition, Timeline, fold};

use crate::state::{TimelineCache, TimelineCacheKey};

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

/// Every durable event of the chain in ascending `chain_seq`, paged through
/// the same store call `events.chain_replay` serves, and the highest seq.
fn load_chain_events(state: &AppState, chain_root_id: &str) -> Result<(i64, Vec<Value>)> {
    let mut events = Vec::new();
    let mut highest = 0;
    let mut after = None;
    loop {
        let page = state.events.replay(&EventReplayParams {
            thread_id: None,
            chain_root_id: Some(chain_root_id.to_string()),
            after_chain_seq: after,
            limit: REPLAY_PAGE_EVENTS,
        })?;
        for record in &page.events {
            highest = highest.max(record.chain_seq);
            events.push(serde_json::to_value(record)?);
        }
        match page.next_cursor {
            Some(cursor) if !page.events.is_empty() => after = Some(cursor),
            _ => return Ok((highest, events)),
        }
    }
}

/// The folded timeline and its highest `chain_seq`. When the chain has no
/// event past the cached seq the cached timeline is reused; otherwise the
/// whole chain is refolded (incremental folding is a later step).
fn timeline_for(
    cache: &TimelineCache,
    key: &TimelineCacheKey,
    has_newer: impl FnOnce(i64) -> Result<bool>,
    load_and_fold: impl FnOnce() -> Result<(i64, Timeline)>,
) -> Result<(i64, Arc<Timeline>)> {
    if let Some((cached_seq, timeline)) = cache.get(key)
        && !has_newer(cached_seq)?
    {
        return Ok((cached_seq, timeline));
    }
    let (highest, timeline) = load_and_fold()?;
    let timeline = Arc::new(timeline);
    cache.insert(key.clone(), (highest, timeline.clone()));
    Ok((highest, timeline))
}

fn window_response(
    chain_root_id: &str,
    definition_ref: &str,
    definition_digest: &str,
    highest_chain_seq: i64,
    timeline: &Timeline,
    limit: usize,
) -> Result<Value> {
    let older = older_count(timeline.items.len(), limit);
    Ok(json!({
        "chain_root_id": chain_root_id,
        "definition_ref": definition_ref,
        "definition_digest": definition_digest,
        "highest_chain_seq": highest_chain_seq,
        "older": older,
        "items": serde_json::to_value(&timeline.items[older..])?,
        "undeclared": serde_json::to_value(&timeline.undeclared)?,
    }))
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
    let (highest, timeline) = timeline_for(
        ui.timeline_cache(),
        &key,
        |cached_seq| {
            let probe = state.events.replay(&EventReplayParams {
                thread_id: None,
                chain_root_id: Some(chain_root_id.clone()),
                after_chain_seq: Some(cached_seq),
                limit: 1,
            })?;
            Ok(!probe.events.is_empty())
        },
        || {
            let (highest, events) = load_chain_events(&state, &chain_root_id)?;
            let timeline = fold(&events, &definition).map_err(HandlerError::Internal)?;
            Ok((highest, timeline))
        },
    )?;
    window_response(
        &chain_root_id,
        &definition_ref,
        &definition_digest,
        highest,
        &timeline,
        limit,
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
    use ryeos_timeline::{Item, Recap};
    use std::collections::BTreeMap;

    fn timeline(items: usize) -> Timeline {
        let items = (0..items)
            .map(|index| {
                Item::Recap(Recap {
                    id: format!("item-{index}"),
                    exchange: "exchange".to_string(),
                    work: 0,
                    duration_s: None,
                    members: Vec::new(),
                })
            })
            .collect();
        Timeline {
            items,
            undeclared: BTreeMap::new(),
        }
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
    fn window_is_the_suffix_and_older_counts_the_rest() {
        assert_eq!(older_count(10, 3), 7);
        assert_eq!(older_count(3, 3), 0);
        assert_eq!(older_count(2, 200), 0);
        let value = window_response("c", "config:x", "d", 9, &timeline(5), 2).unwrap();
        assert_eq!(value["older"], 3);
        assert_eq!(value["highest_chain_seq"], 9);
        let items = value["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], "item-3");
        assert_eq!(items[1]["id"], "item-4");
    }

    #[test]
    fn cache_reuses_until_the_chain_grows_then_refolds() {
        let cache = TimelineCache::with_capacity(4);
        let key = ("chain".to_string(), "digest".to_string());
        let folds = std::cell::Cell::new(0);
        let fold_at = |seq: i64| {
            folds.set(folds.get() + 1);
            Ok((seq, timeline(seq as usize)))
        };

        let (seq, _) = timeline_for(&cache, &key, |_| unreachable!(), || fold_at(3)).unwrap();
        assert_eq!(seq, 3);
        assert_eq!(folds.get(), 1);

        let (seq, _) = timeline_for(&cache, &key, |cached| Ok(cached < 3), || fold_at(9)).unwrap();
        assert_eq!(seq, 3, "unchanged seq reuses the cached fold");
        assert_eq!(folds.get(), 1);

        let (seq, timeline) = timeline_for(&cache, &key, |_| Ok(true), || fold_at(5)).unwrap();
        assert_eq!(seq, 5);
        assert_eq!(timeline.items.len(), 5);
        assert_eq!(folds.get(), 2);

        let other = ("chain".to_string(), "other-digest".to_string());
        timeline_for(&cache, &other, |_| unreachable!(), || fold_at(1)).unwrap();
        assert_eq!(folds.get(), 3, "a different definition digest misses");
    }
}
