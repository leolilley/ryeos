//! `ui.cockpit.items.list` and `ui.cockpit.item.inspect` — item inventory
//! and inspection for the 2D cockpit.
//!
//! These are read-only endpoints. Items are enumerated from the engine's
//! kind registry and resolution roots. No pseudo item kinds are introduced.
//!
//! `items.list`  — enumerate real schema-backed items with filtering.
//! `item.inspect` — resolve one canonical item and return raw + effective.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use ryeos_api::registry::ServiceDescriptor;
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::ItemSpace;
use ryeos_engine::item_resolution::{enumerate_kind_refs, resolve_item_full};
use ryeos_executor::executor::ServiceAvailability;

use super::ui_graph_topology::{classify_trust, label_for_bare_id, namespace_for_bare_id};
use crate::state::get_ui_state;

// ── items.list types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemsListRequest {
    /// Optional kind filter (e.g. "tool", "directive").
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional space filter ("project", "user", "system").
    #[serde(default)]
    pub space: Option<String>,
    /// Optional substring search on bare_id or label.
    #[serde(default)]
    pub query: Option<String>,
    /// Max items to return.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Include shadowed (lower-priority) candidates per item.
    #[serde(default)]
    pub include_shadowed: bool,
}

fn default_limit() -> usize {
    500
}

const MAX_ITEMS_LIMIT: usize = 2_000;

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemsListResponse {
    pub schema_version: &'static str,
    pub counts: ItemsCounts,
    pub items: Vec<ItemSummary>,
}

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemsCounts {
    pub by_kind: BTreeMap<String, usize>,
    pub by_space: BTreeMap<String, usize>,
}

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemSummary {
    pub domain: &'static str,
    pub canonical_ref: String,
    pub item_kind: String,
    pub bare_id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub space: String,
    pub source_path: String,
    pub executable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<super::ui_graph_topology::TrustSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shadowed: Vec<ShadowedSummary>,
}

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowedSummary {
    pub space: String,
    pub label: String,
    pub path: String,
}

// ── item.inspect types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemInspectRequest {
    pub canonical_ref: String,
    /// Include raw source content.
    #[serde(default = "default_true")]
    pub include_raw: bool,
    /// Include effective (composed) content.
    #[serde(default = "default_true")]
    pub include_effective: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemInspectResponse {
    pub schema_version: &'static str,
    pub item: InspectedItem,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<RawContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective: Option<Value>,
}

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct InspectedItem {
    pub canonical_ref: String,
    pub item_kind: String,
    pub bare_id: String,
    pub source_path: String,
    pub space: String,
    pub executable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust: Option<super::ui_graph_topology::TrustSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shadowed: Vec<ShadowedSummary>,
}

#[derive(Debug, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawContent {
    pub content: String,
    pub bytes: usize,
    pub truncated: bool,
}

/// Max raw content size (256 KiB).
const MAX_RAW_BYTES: usize = 256 * 1024;

// ── items.list handler ─────────────────────────────────────────────

fn session_id_from_context(ctx: &HandlerContext) -> Option<String> {
    ctx.fingerprint.strip_prefix("session:").map(String::from)
}

pub async fn handle_items_list(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;

    let session = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

    let project_path = session.project_root.as_ref().map(PathBuf::from);
    let request: ItemsListRequest = if params.is_null() {
        ItemsListRequest {
            kind: None,
            space: None,
            query: None,
            limit: default_limit(),
            include_shadowed: false,
        }
    } else {
        serde_json::from_value(params)
            .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?
    };

    let response = build_items_list(&state, project_path.as_ref(), &request);

    serde_json::to_value(response).map_err(Into::into)
}

fn build_items_list(
    state: &AppState,
    project_path: Option<&PathBuf>,
    req: &ItemsListRequest,
) -> ItemsListResponse {
    let roots = state.engine.resolution_roots(project_path.cloned());

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_space: BTreeMap<String, usize> = BTreeMap::new();
    let mut items: Vec<ItemSummary> = Vec::new();

    let kind_filter = req.kind.as_deref();
    let space_filter = req.space.as_deref().map(parse_space_filter);
    let query_lower = req.query.as_deref().map(str::to_lowercase);
    let limit = req.limit.clamp(1, MAX_ITEMS_LIMIT);

    for kind_name in state.engine.kinds.kinds() {
        // Apply kind filter
        if let Some(filter) = kind_filter {
            if kind_name != filter {
                continue;
            }
        }

        let Some(schema) = state.engine.kinds.get(kind_name) else {
            continue;
        };

        let kind_refs = enumerate_kind_refs(&roots, schema, kind_name);
        let executable = schema.is_executable();

        for item_ref in &kind_refs {
            let canonical = item_ref.to_string();

            let resolution = match resolve_item_full(&roots, schema, item_ref) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let space_str = resolution.winner_space.as_str().to_string();

            // Apply space filter
            if let Some(filter_fn) = &space_filter {
                if !filter_fn(&resolution.winner_space) {
                    continue;
                }
            }

            // Apply query filter
            if let Some(ref q) = query_lower {
                let bare_lower = item_ref.bare_id.to_lowercase();
                let label_lower = label_for_bare_id(&item_ref.bare_id).to_lowercase();
                if !bare_lower.contains(q) && !label_lower.contains(q) {
                    continue;
                }
            }

            // Counts (count all matching items before limit)
            *by_kind.entry(kind_name.to_string()).or_insert(0) += 1;
            *by_space.entry(space_str.clone()).or_insert(0) += 1;

            // Trust classification
            let trust = classify_trust(
                &resolution.winner_path,
                schema
                    .spec_for(&resolution.matched_ext)
                    .map(|spec| &spec.signature),
                &state.engine.trust_store,
            );

            // Shadowed candidates
            let shadowed = if req.include_shadowed {
                resolution
                    .shadowed
                    .iter()
                    .map(|s| ShadowedSummary {
                        space: s.space.as_str().to_string(),
                        label: s.label.clone(),
                        path: s.path.display().to_string(),
                    })
                    .collect()
            } else {
                Vec::new()
            };

            items.push(ItemSummary {
                domain: "item",
                canonical_ref: canonical.clone(),
                item_kind: kind_name.to_string(),
                bare_id: item_ref.bare_id.clone(),
                label: label_for_bare_id(&item_ref.bare_id),
                namespace: namespace_for_bare_id(&item_ref.bare_id),
                space: space_str,
                source_path: resolution.winner_path.display().to_string(),
                executable,
                trust,
                shadowed,
            });

            if items.len() >= limit {
                break;
            }
        }

        if items.len() >= limit {
            break;
        }
    }

    ItemsListResponse {
        schema_version: "cockpit.items.v1",
        counts: ItemsCounts { by_kind, by_space },
        items,
    }
}

fn parse_space_filter(s: &str) -> impl Fn(&ItemSpace) -> bool + '_ {
    move |space: &ItemSpace| space.as_str() == s
}

// ── item.inspect handler ───────────────────────────────────────────

pub async fn handle_item_inspect(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    let session_id = session_id_from_context(&ctx)
        .ok_or_else(|| HandlerError::Forbidden("browser session required".into()))?;

    let session = get_ui_state(&state)
        .expect("UiState not set")
        .browser_sessions
        .get_session(&session_id)
        .ok_or(HandlerError::Forbidden("session expired or invalid".into()))?;

    let req: ItemInspectRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let project_path = session.project_root.as_ref().map(PathBuf::from);
    let response = build_item_inspect(&state, project_path.as_ref(), &req)?;

    serde_json::to_value(response).map_err(Into::into)
}

fn build_item_inspect(
    state: &AppState,
    project_path: Option<&PathBuf>,
    req: &ItemInspectRequest,
) -> Result<ItemInspectResponse> {
    let item_ref = CanonicalRef::parse(&req.canonical_ref).map_err(|e| {
        HandlerError::BadRequest(format!(
            "invalid canonical ref '{}': {e}",
            req.canonical_ref
        ))
    })?;

    let roots = state.engine.resolution_roots(project_path.cloned());

    let Some(schema) = state.engine.kinds.get(&item_ref.kind) else {
        return Err(HandlerError::NotFound.into());
    };

    let resolution =
        resolve_item_full(&roots, schema, &item_ref).map_err(|_| HandlerError::NotFound)?;

    let space_str = resolution.winner_space.as_str().to_string();
    let executable = schema.is_executable();

    // Trust classification
    let trust = classify_trust(
        &resolution.winner_path,
        schema
            .spec_for(&resolution.matched_ext)
            .map(|spec| &spec.signature),
        &state.engine.trust_store,
    );

    // Shadowed
    let shadowed: Vec<ShadowedSummary> = resolution
        .shadowed
        .iter()
        .map(|s| ShadowedSummary {
            space: s.space.as_str().to_string(),
            label: s.label.clone(),
            path: s.path.display().to_string(),
        })
        .collect();

    // Raw content
    let raw = if req.include_raw {
        read_raw_content(&resolution.winner_path)
    } else {
        None
    };

    // Effective content
    let effective = if req.include_effective {
        let effective_req = ryeos_engine::engine::EffectiveItemRequest {
            item_ref: item_ref.clone(),
            expected_kind: None,
            project_root: project_path.cloned(),
        };
        state
            .engine
            .effective_item(effective_req)
            .ok()
            .and_then(|eff| serde_json::to_value(eff).ok())
    } else {
        None
    };

    Ok(ItemInspectResponse {
        schema_version: "cockpit.item.inspect.v1",
        item: InspectedItem {
            canonical_ref: req.canonical_ref.clone(),
            item_kind: item_ref.kind.clone(),
            bare_id: item_ref.bare_id.clone(),
            source_path: resolution.winner_path.display().to_string(),
            space: space_str,
            executable,
            trust,
            shadowed,
        },
        raw,
        effective,
    })
}

fn read_raw_content(path: &std::path::Path) -> Option<RawContent> {
    let actual_bytes = std::fs::metadata(path).ok().map(|m| m.len() as usize);
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(MAX_RAW_BYTES.saturating_add(1));
    file.take(MAX_RAW_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;

    let observed_bytes = bytes.len();
    let total_bytes = actual_bytes.unwrap_or(observed_bytes);
    let truncated = total_bytes > MAX_RAW_BYTES || observed_bytes > MAX_RAW_BYTES;
    if bytes.len() > MAX_RAW_BYTES {
        bytes.truncate(MAX_RAW_BYTES);
    }

    Some(RawContent {
        content: String::from_utf8_lossy(&bytes).into_owned(),
        bytes: total_bytes,
        truncated,
    })
}

// ── Descriptors ────────────────────────────────────────────────────

pub const ITEMS_LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/cockpit/items/list",
    endpoint: "ui.cockpit.items.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_items_list(params, ctx, state).await })
    },
};

pub const ITEM_INSPECT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/cockpit/item/inspect",
    endpoint: "ui.cockpit.item.inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_item_inspect(params, ctx, state).await })
    },
};
