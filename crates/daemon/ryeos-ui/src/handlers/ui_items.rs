//! `ui.ryeos.items.list` and `ui.ryeos.item.inspect` — item inventory
//! and inspection for the 2D ryeos-ui.
//!
//! These are read-only endpoints. Items are enumerated from the engine's
//! kind registry and resolution roots. No pseudo item kinds are introduced.
//!
//! `items.list`  — enumerate real schema-backed items with filtering.
//! `item.inspect` — resolve one canonical item and return raw + effective.

use std::collections::BTreeMap;
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
use ryeos_engine::error::EngineError;
use ryeos_engine::item_resolution::{resolve_item_full, try_enumerate_kind_refs};
use ryeos_executor::executor::ServiceAvailability;

use super::ui_graph_topology::{classify_trust, label_for_bare_id, namespace_for_bare_id};

// ── items.list types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemsListRequest {
    /// Optional kind filter (e.g. "tool", "directive").
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional general authored-item space ("project" or "bundle").
    #[serde(default)]
    pub space: Option<ItemSpace>,
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
    pub coverage: ItemsCoverage,
}

#[derive(Debug, serde::Serialize)]
pub struct ItemsCoverage {
    pub state: &'static str,
    pub reason: &'static str,
    pub returned: usize,
    pub limit: usize,
    pub resolution_failures: usize,
    pub traversal_failures: usize,
    pub limit_reached: bool,
    pub counts_scope: &'static str,
}

fn items_coverage(
    returned: usize,
    limit: usize,
    resolution_failures: usize,
    traversal_failures: usize,
) -> ItemsCoverage {
    let limit_reached = returned >= limit;
    ItemsCoverage {
        state: if returned == 0 && (resolution_failures > 0 || traversal_failures > 0) {
            "failed"
        } else if limit_reached || resolution_failures > 0 || traversal_failures > 0 {
            "partial"
        } else {
            "observed"
        },
        // Discovery resolves a bounded live corpus, not an atomic snapshot.
        reason: "bounded_live_enumeration",
        returned,
        limit,
        resolution_failures,
        traversal_failures,
        limit_reached,
        counts_scope: "returned_items",
    }
}

fn unavailable_kind_coverage(limit: usize) -> ItemsCoverage {
    ItemsCoverage {
        state: "unavailable",
        reason: "kind_not_registered",
        ..items_coverage(0, limit, 0, 0)
    }
}

fn validate_catalogue_space(space: Option<ItemSpace>) -> Result<(), HandlerError> {
    if space == Some(ItemSpace::Node) {
        return Err(HandlerError::BadRequest(
            "node-local configuration is not a general item catalogue space".into(),
        ));
    }
    Ok(())
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
    pub raw_coverage: ContentCoverage,
    pub effective_coverage: ContentCoverage,
}

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
pub struct ContentCoverage {
    pub state: &'static str,
    pub reason: &'static str,
}

fn requested_content<T>(
    requested: bool,
    load: impl FnOnce() -> Result<T>,
) -> (Option<T>, ContentCoverage) {
    if !requested {
        return (
            None,
            ContentCoverage {
                state: "not_requested",
                reason: "not_requested",
            },
        );
    }
    match load() {
        Ok(value) => (
            Some(value),
            ContentCoverage {
                state: "available",
                reason: "read_succeeded",
            },
        ),
        // Do not leak paths or internal composition causes through a broad read
        // projection; failure is distinct from absence and from not requested.
        Err(_) => (
            None,
            ContentCoverage {
                state: "failed",
                reason: "read_failed",
            },
        ),
    }
}

/// A browser parameter is a consistency hint, never filesystem authority.
/// Signed operators have already passed require_admitted_operator; keep that
/// existing explicit-path lane separate from compiled browser attachments.
fn requested_project_path(
    supplied: Option<&Value>,
    operator: bool,
    admitted_identity: Option<&std::path::Path>,
) -> Result<Option<PathBuf>> {
    let supplied = match supplied {
        None | Some(Value::Null) => None,
        Some(Value::String(path)) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => {
            return Err(
                HandlerError::BadRequest("project_path must be a nonempty string".into()).into(),
            );
        }
    };
    if operator {
        return Ok(supplied);
    }
    if supplied
        .as_deref()
        .is_some_and(|path| Some(path) != admitted_identity)
    {
        return Err(HandlerError::Forbidden(
            "project_path does not match admitted project authority".into(),
        )
        .into());
    }
    Ok(None)
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

pub async fn handle_items_list(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    // Dual-lane (browser session OR verified operator) like the other
    // ui/ryeos-ui sources — the TUI calls as a signed operator.
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let project_access = caller.project_access()?;
    let operator_path = requested_project_path(
        params.get("project_path"),
        matches!(&caller, crate::seat_auth::SeatCaller::Operator { .. }),
        caller.project_query_identity()?.as_deref(),
    )?;
    let project_path: Option<PathBuf> = project_access
        .as_ref()
        .map(|access| access.path().to_path_buf())
        .or(operator_path);
    let mut params = params;
    if let Some(map) = params.as_object_mut() {
        map.remove("project_path");
    }
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

    validate_catalogue_space(request.space)?;
    let response = build_items_list(&state, project_path.as_ref(), &request);

    serde_json::to_value(response).map_err(Into::into)
}

fn build_items_list(
    state: &AppState,
    project_path: Option<&PathBuf>,
    req: &ItemsListRequest,
) -> ItemsListResponse {
    let limit = req.limit.clamp(1, MAX_ITEMS_LIMIT);
    if req
        .kind
        .as_deref()
        .is_some_and(|kind| state.engine.kinds.get(kind).is_none())
    {
        return ItemsListResponse {
            schema_version: "ryeos.ui.items.v1",
            counts: ItemsCounts {
                by_kind: BTreeMap::new(),
                by_space: BTreeMap::new(),
            },
            items: Vec::new(),
            coverage: unavailable_kind_coverage(limit),
        };
    }
    let roots = state.engine.resolution_roots(project_path.cloned());

    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_space: BTreeMap<String, usize> = BTreeMap::new();
    let mut items: Vec<ItemSummary> = Vec::new();
    let mut resolution_failures = 0;
    let mut traversal_failures = 0;

    let kind_filter = req.kind.as_deref();
    let space_filter = req.space;
    let query_lower = req.query.as_deref().map(str::to_lowercase);

    for kind_name in state.engine.kinds.kinds() {
        // Apply kind filter
        if let Some(filter) = kind_filter
            && kind_name != filter
        {
            continue;
        }

        let Some(schema) = state.engine.kinds.get(kind_name) else {
            continue;
        };

        let kind_refs = match try_enumerate_kind_refs(&roots, schema, kind_name) {
            Ok(refs) => refs,
            Err(_) => {
                traversal_failures += 1;
                continue;
            }
        };
        let executable = schema.is_executable();

        for item_ref in &kind_refs {
            let canonical = item_ref.to_string();

            let resolution = match resolve_item_full(&roots, schema, item_ref) {
                Ok(r) => r,
                Err(_) => {
                    resolution_failures += 1;
                    continue;
                }
            };

            let space_str = resolution.winner_space.as_str().to_string();

            // Apply space filter
            if let Some(space) = space_filter
                && resolution.winner_space != space
            {
                continue;
            }

            // Apply query filter
            if let Some(ref q) = query_lower {
                let bare_lower = item_ref.bare_id.to_lowercase();
                let label_lower = label_for_bare_id(&item_ref.bare_id).to_lowercase();
                if !bare_lower.contains(q) && !label_lower.contains(q) {
                    continue;
                }
            }

            // Counts describe only returned items, not the full catalogue.
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
        schema_version: "ryeos.ui.items.v1",
        counts: ItemsCounts { by_kind, by_space },
        coverage: items_coverage(items.len(), limit, resolution_failures, traversal_failures),
        items,
    }
}

// ── item.inspect handler ───────────────────────────────────────────

pub async fn handle_item_inspect(
    params: Value,
    ctx: HandlerContext,
    state: Arc<AppState>,
) -> Result<Value> {
    // Dual-lane (browser session OR verified operator) like the other
    // ui/ryeos-ui sources — the TUI calls as a signed operator.
    let caller = crate::seat_auth::require_seat_caller(&ctx, &state)?;
    let project_access = caller.project_access()?;
    let operator_path = requested_project_path(
        params.get("project_path"),
        matches!(&caller, crate::seat_auth::SeatCaller::Operator { .. }),
        caller.project_query_identity()?.as_deref(),
    )?;
    let project_path: Option<PathBuf> = project_access
        .as_ref()
        .map(|access| access.path().to_path_buf())
        .or(operator_path);
    let mut params = params;
    if let Some(map) = params.as_object_mut() {
        map.remove("project_path");
    }
    let req: ItemInspectRequest = serde_json::from_value(params)
        .map_err(|e| HandlerError::BadRequest(format!("invalid request: {e}")))?;

    let response =
        build_item_inspect(&state, project_path.as_ref(), project_access.as_ref(), &req)?;

    serde_json::to_value(response).map_err(Into::into)
}

fn build_item_inspect(
    state: &AppState,
    project_path: Option<&PathBuf>,
    project_access: Option<&crate::seat_auth::RetainedProjectAccess>,
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
        resolve_item_full(&roots, schema, &item_ref).map_err(inspect_resolution_error)?;

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
    let (raw, raw_coverage) = requested_content(req.include_raw, || {
        let project_directory = project_access
            .map(|access| access.try_clone_directory())
            .transpose()?;
        let project = project_access
            .zip(project_directory.as_ref())
            .map(|(access, directory)| (access.path(), directory));
        read_raw_content(&resolution.winner_path, project)
    });

    // Effective content
    let (effective, effective_coverage) = requested_content(req.include_effective, || {
        let effective_req = ryeos_engine::engine::EffectiveItemRequest {
            item_ref: item_ref.clone(),
            expected_kind: None,
            project_root: project_path.cloned(),
            subject_resolution_authority:
                ryeos_engine::contracts::SubjectResolutionAuthority::for_live_project_root(
                    project_path.map(std::path::PathBuf::as_path),
                ),
        };
        Ok(serde_json::to_value(
            state.engine.effective_item(effective_req)?,
        )?)
    });

    Ok(ItemInspectResponse {
        schema_version: "ryeos-ui.item.inspect.v1",
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
        raw_coverage,
        effective_coverage,
    })
}

fn inspect_resolution_error(error: EngineError) -> HandlerError {
    let mut cause = &error;
    while let EngineError::Shared(inner) = cause {
        cause = inner.as_ref();
    }
    if matches!(cause, EngineError::ItemNotFound { .. }) {
        HandlerError::NotFound
    } else {
        // An unavailable resolution is not proof of absence. Keep the private
        // filesystem cause in server diagnostics, not the browser response.
        tracing::error!(error = %error, "item inspection resolution failed");
        HandlerError::Internal("item inspection resolution unavailable".into())
    }
}

fn read_raw_content(
    path: &std::path::Path,
    project: Option<(&std::path::Path, &lillux::PinnedDirectory)>,
) -> Result<RawContent> {
    // The descriptor-rooted coordinate belongs to the retained project owner;
    // reopening its synthetic path would either follow a link or reject valid
    // admitted access. Traverse only its relative descendant through that owner.
    let file =
        match project.and_then(|(root, directory)| {
            path.strip_prefix(root)
                .ok()
                .map(|relative| (relative, directory))
        }) {
            Some((relative, directory)) => directory
                .open_pinned_regular_descendant(relative, false)?
                .ok_or_else(|| anyhow::anyhow!("raw item no longer exists"))?,
            None => lillux::secure_fs::open_pinned_regular_file_no_follow(path)?,
        };
    let observation = file.observation()?;
    // The existing stable-read owner provides complete bounded reads, not a
    // preview. Oversized/changing files therefore explicitly fail raw coverage.
    let bytes = file.read_stable_bounded(&observation, MAX_RAW_BYTES as u64)?;

    Ok(RawContent {
        content: String::from_utf8_lossy(&bytes).into_owned(),
        bytes: bytes.len(),
        truncated: false,
    })
}

// ── Descriptors ────────────────────────────────────────────────────

pub const ITEMS_LIST_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/items/list",
    endpoint: "ui.ryeos.items.list",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_items_list(params, ctx, state).await })
    },
};

pub const ITEM_INSPECT_DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:ui/ryeos-ui/item/inspect",
    endpoint: "ui.ryeos.item.inspect",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &[],
    handler: |params, ctx, state| {
        Box::pin(async move { handle_item_inspect(params, ctx, state).await })
    },
};

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn catalogue_filters_use_existing_general_item_spaces() {
        for (value, expected) in [
            ("project", ItemSpace::Project),
            ("bundle", ItemSpace::Bundle),
        ] {
            let request: ItemsListRequest = serde_json::from_value(json!({"space":value})).unwrap();
            assert_eq!(request.space, Some(expected));
            validate_catalogue_space(request.space).unwrap();
        }
        for value in ["user", "system", "unknown", ""] {
            assert!(serde_json::from_value::<ItemsListRequest>(json!({"space":value})).is_err());
        }
        let node: ItemsListRequest = serde_json::from_value(json!({"space":"node"})).unwrap();
        assert!(matches!(
            validate_catalogue_space(node.space),
            Err(HandlerError::BadRequest(_))
        ));
        validate_catalogue_space(None).unwrap();
    }

    #[test]
    fn unregistered_kind_is_not_a_successful_empty_corpus_or_traversal_failure() {
        let unavailable = unavailable_kind_coverage(200);
        assert_eq!(unavailable.state, "unavailable");
        assert_eq!(unavailable.reason, "kind_not_registered");
        assert_eq!(unavailable.returned, 0);
        assert_eq!(unavailable.limit, 200);
        assert_eq!(unavailable.resolution_failures, 0);
        assert_eq!(unavailable.traversal_failures, 0);
        assert!(!unavailable.limit_reached);
        assert_eq!(items_coverage(0, 200, 0, 0).state, "observed");
    }

    #[test]
    fn inspection_preserves_only_authoritative_item_absence_as_not_found() {
        let absent = || EngineError::ItemNotFound {
            canonical_ref: "surface:test/missing".into(),
            searched_spaces: vec!["bundle".into()],
        };
        assert!(matches!(
            inspect_resolution_error(absent()),
            HandlerError::NotFound
        ));
        assert!(matches!(
            inspect_resolution_error(EngineError::Shared(Arc::new(absent()))),
            HandlerError::NotFound
        ));
        let unavailable = || EngineError::ItemResolutionUnavailable {
            canonical_ref: "surface:test/private".into(),
            path: PathBuf::from("/private/resolution-root"),
            source: std::io::Error::other("private read failure"),
        };
        for error in [
            unavailable(),
            EngineError::Shared(Arc::new(unavailable())),
            EngineError::InvalidProjectContext {
                reason: "private project cause".into(),
            },
        ] {
            let public = inspect_resolution_error(error);
            assert!(matches!(&public, HandlerError::Internal(_)));
            assert_eq!(
                public.to_string(),
                "internal: item inspection resolution unavailable"
            );
            assert!(!public.to_string().contains("private"));
        }
    }

    #[test]
    fn projectless_attachment_cannot_supply_filesystem_authority() {
        assert!(requested_project_path(Some(&json!("/project")), false, None).is_err());
        assert_eq!(requested_project_path(None, false, None).unwrap(), None);
    }

    #[test]
    fn attachment_project_hint_must_match_and_never_becomes_access_path() {
        let admitted = Some(Path::new("/admitted"));
        assert_eq!(
            requested_project_path(Some(&json!("/admitted")), false, admitted).unwrap(),
            None
        );
        assert!(requested_project_path(Some(&json!("/other")), false, admitted).is_err());
        assert!(requested_project_path(Some(&json!(42)), false, admitted).is_err());
    }

    #[test]
    fn admitted_operator_keeps_explicit_project_resolution() {
        assert_eq!(
            requested_project_path(Some(&json!("/project")), true, None).unwrap(),
            Some(PathBuf::from("/project"))
        );
    }

    #[test]
    fn catalogue_coverage_never_claims_exhaustive_empty_or_total_counts() {
        let empty = items_coverage(0, 20, 0, 0);
        assert_eq!(empty.state, "observed");
        assert_eq!(empty.reason, "bounded_live_enumeration");
        assert_eq!(empty.counts_scope, "returned_items");
        let bounded = items_coverage(20, 20, 0, 0);
        assert_eq!(bounded.state, "partial");
        assert!(bounded.limit_reached);
        let failed = items_coverage(0, 20, 2, 0);
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.resolution_failures, 2);
        assert!(!failed.limit_reached);
        assert_eq!(items_coverage(0, 20, 0, 1).state, "failed");
        assert_eq!(items_coverage(1, 20, 0, 1).state, "partial");
    }

    #[test]
    fn content_failure_is_distinct_from_not_requested_and_available_null() {
        let (_, skipped) = requested_content::<Value>(false, || panic!("must not load"));
        assert_eq!(skipped.state, "not_requested");
        let (value, failed) = requested_content::<Value>(true, || anyhow::bail!("private cause"));
        assert!(value.is_none());
        assert_eq!(failed.state, "failed");
        assert!(
            !serde_json::to_string(&failed)
                .unwrap()
                .contains("private cause")
        );
        let (value, available) = requested_content(true, || Ok(Value::Null));
        assert_eq!(value, Some(Value::Null));
        assert_eq!(available.state, "available");
    }

    #[test]
    fn raw_read_errors_are_not_empty_content() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_raw_content(&directory.path().join("missing"), None).is_err());
        let path = directory.path().join("content");
        std::fs::write(&path, vec![b'x'; MAX_RAW_BYTES + 7]).unwrap();
        assert!(read_raw_content(&path, None).is_err());
        std::fs::write(&path, b"bounded").unwrap();
        let raw = read_raw_content(&path, None).unwrap();
        assert!(!raw.truncated);
        assert_eq!(raw.content, "bounded");
        assert_eq!(raw.bytes, 7);
    }

    #[test]
    fn raw_read_uses_admitted_descriptor_and_refuses_symlink_substitution() {
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        let coordinate = directory.descriptor_path().unwrap();
        let file = root.path().join("item");
        std::fs::write(&file, b"admitted").unwrap();
        let synthetic = coordinate.join("item");
        assert_eq!(
            read_raw_content(&synthetic, Some((&coordinate, &directory)))
                .unwrap()
                .content,
            "admitted"
        );
        std::fs::remove_file(&file).unwrap();
        std::fs::write(root.path().join("private"), b"must not read").unwrap();
        directory
            .create_symlink(std::ffi::OsStr::new("item"), b"private")
            .unwrap();
        assert!(read_raw_content(&synthetic, Some((&coordinate, &directory))).is_err());
        assert!(read_raw_content(&file, None).is_err());
    }
}
