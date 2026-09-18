//! Server-side view embedding for effective surfaces.
//!
//! A surface binds views by `view:` ref; it never defines them. Every
//! daemon path that serves an effective surface (`items.effective`,
//! `ui.session.current`) embeds the composed value of each bound view
//! into the surface's `views` map before the payload leaves the daemon,
//! so renderers receive one complete surface instead of resolving each
//! ref in a follow-up round-trip.
//!
//! A view that fails to resolve embeds `{"degraded": <reason>}` under
//! its ref — the surface still ships, and the pane renders the reason
//! instead of the view. Per-view failures never fail the whole surface.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::contracts::SubjectResolutionAuthority;
use ryeos_engine::engine::{CheckedEngineGeneration, EffectiveItem, EffectiveItemRequest, Engine};
use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

/// Engine-owned effective identity retained beside one UI item's composed
/// presentation. The digest commits the complete effective resolution; this
/// API never derives identity from the presentation JSON it embeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveUiItemIdentity {
    pub canonical_ref: String,
    pub effective_definition_digest: EffectiveDefinitionDigest,
    /// Effective weakest-link trust result computed by the engine. Binding
    /// admission consumes this explicitly; it must not infer trust from the
    /// definition digest or the presentation payload.
    pub effective_trust_class: TrustClass,
}

impl EffectiveUiItemIdentity {
    fn from_effective_item(item: &EffectiveItem) -> Self {
        Self {
            canonical_ref: item.canonical_ref.clone(),
            effective_definition_digest: item.effective_definition_digest.clone(),
            effective_trust_class: item.trust_class,
        }
    }
}

/// Resolution state for one view named by the surface. A failed view remains
/// in both the presentation as a degraded placeholder and this sidecar as a
/// degraded identity record; failure never fabricates definition identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddedViewIdentity {
    Resolved { identity: EffectiveUiItemIdentity },
    Degraded { reason: String },
}

/// Exact surface/view identity closure retained beside the unchanged composed
/// surface presentation. This is input to later binding compilation, not a
/// browser-authored provenance format or an independent source of authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedSurfaceIdentity {
    pub request_engine_generation_identity: String,
    pub surface: EffectiveUiItemIdentity,
    pub views: BTreeMap<String, EmbeddedViewIdentity>,
}

/// Result of one generation-coherent surface/view embedding operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedSurfaceViews {
    pub identity: EmbeddedSurfaceIdentity,
    pub failures: Vec<(String, String)>,
}

/// Collect every `view:`-prefixed ref anywhere in the composed surface
/// value — center `tiles`, edge `slots`, `backdrop`, `library` — skipping
/// the ROOT `views` map only (it holds resolved bindings keyed by ref,
/// not refs to resolve). The skip must not apply to nested `views` keys:
/// each grouped `library` entry is `{ group, views: [ref…] }`, and
/// skipping those lists is exactly how a grouped launcher loses every
/// declared view.
fn collect_view_refs(value: &Value, out: &mut Vec<String>) {
    collect_view_refs_at(value, out, true);
}

fn unique_view_refs(value: &Value) -> Vec<String> {
    let mut view_refs = Vec::new();
    collect_view_refs(value, &mut view_refs);
    view_refs.sort();
    view_refs.dedup();
    view_refs
}

fn collect_view_refs_at(value: &Value, out: &mut Vec<String>, at_root: bool) {
    match value {
        Value::String(s) if s.starts_with("view:") => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                collect_view_refs_at(item, out, false);
            }
        }
        Value::Object(map) => {
            for (key, v) in map {
                if at_root && key == "views" {
                    continue;
                }
                collect_view_refs_at(v, out, false);
            }
        }
        _ => {}
    }
}

/// Embed every bound view into `composed_value.views`, resolving each
/// unique ref through `resolve`. A failed resolution embeds a
/// `{"degraded": <reason>}` entry under the same key. Returns the
/// failures as `(view_ref, reason)` pairs so callers can additionally
/// report them as diagnostics.
pub fn embed_views_with(
    composed_value: &mut Value,
    mut resolve: impl FnMut(&str) -> Result<Value, String>,
) -> Vec<(String, String)> {
    if !composed_value.is_object() {
        return Vec::new();
    }
    let view_refs = unique_view_refs(composed_value);
    if view_refs.is_empty() {
        return Vec::new();
    }

    let resolved = view_refs
        .into_iter()
        .map(|view_ref| {
            let result = resolve(&view_ref);
            (view_ref, result)
        })
        .collect();
    embed_view_results(composed_value, resolved)
}

fn embed_view_results(
    composed_value: &mut Value,
    resolved: Vec<(String, Result<Value, String>)>,
) -> Vec<(String, String)> {
    // `views` is never authored inline; anything non-object there is
    // malformed. Reset it so per-ref insertion below cannot panic.
    if composed_value.get("views").is_some_and(|v| !v.is_object()) {
        composed_value["views"] = Value::Object(serde_json::Map::new());
    }

    let mut failures: Vec<(String, String)> = Vec::new();
    for (view_ref, result) in resolved {
        match result {
            Ok(view_value) => {
                composed_value["views"][&view_ref] = view_value;
            }
            Err(reason) => {
                composed_value["views"][&view_ref] =
                    serde_json::json!({ "degraded": reason.clone() });
                failures.push((view_ref, reason));
            }
        }
    }
    failures
}

fn embed_identity_view_results(
    composed_value: &mut Value,
    request_engine_generation_identity: String,
    surface: EffectiveUiItemIdentity,
    resolved: Vec<(String, Result<(Value, EffectiveUiItemIdentity), String>)>,
) -> EmbeddedSurfaceViews {
    if composed_value.get("views").is_some_and(|v| !v.is_object()) {
        composed_value["views"] = Value::Object(serde_json::Map::new());
    }

    let mut views = BTreeMap::new();
    let mut failures = Vec::new();
    for (view_ref, result) in resolved {
        match result {
            Ok((view_value, identity)) => {
                composed_value["views"][&view_ref] = view_value;
                views.insert(view_ref, EmbeddedViewIdentity::Resolved { identity });
            }
            Err(reason) => {
                composed_value["views"][&view_ref] =
                    serde_json::json!({ "degraded": reason.clone() });
                views.insert(
                    view_ref.clone(),
                    EmbeddedViewIdentity::Degraded {
                        reason: reason.clone(),
                    },
                );
                failures.push((view_ref, reason));
            }
        }
    }

    EmbeddedSurfaceViews {
        identity: EmbeddedSurfaceIdentity {
            request_engine_generation_identity,
            surface,
            views,
        },
        failures,
    }
}

/// Resolve each bound `view:` ref through the engine's effective-item
/// pipeline and embed the composed view values into the surface.
pub fn embed_surface_views(
    engine: &Engine,
    project_root: Option<&Path>,
    composed_value: &mut Value,
) -> Vec<(String, String)> {
    embed_views_with(composed_value, |view_ref| {
        let item_ref =
            CanonicalRef::parse(view_ref).map_err(|e| format!("invalid view ref: {e}"))?;
        engine
            .effective_item(EffectiveItemRequest {
                item_ref,
                expected_kind: Some("view".to_string()),
                project_root: project_root.map(Path::to_path_buf),
                subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
                    project_root,
                ),
            })
            .map(|effective| effective.composed_value)
            .map_err(|e| e.to_string())
    })
}

/// Resolve a surface's bound views inside a caller-owned checked generation
/// batch. This avoids reacquiring and re-verifying the installed bundle
/// generation for every view while preserving the same resolution semantics.
pub fn embed_surface_views_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    composed_value: &mut Value,
) -> Vec<(String, String)> {
    if !composed_value.is_object() {
        return Vec::new();
    }
    let mut resolved = Vec::new();
    let mut requests = Vec::new();
    let mut valid_refs = Vec::new();
    for view_ref in unique_view_refs(composed_value) {
        match CanonicalRef::parse(&view_ref) {
            Ok(item_ref) => {
                valid_refs.push(view_ref);
                requests.push(EffectiveItemRequest {
                    item_ref,
                    expected_kind: Some("view".to_string()),
                    project_root: project_root.map(Path::to_path_buf),
                    subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
                        project_root,
                    ),
                });
            }
            Err(error) => resolved.push((view_ref, Err(format!("invalid view ref: {error}")))),
        }
    }
    resolved.extend(
        valid_refs
            .into_iter()
            .zip(generation.effective_items(&requests))
            .map(|(view_ref, result)| {
                (
                    view_ref,
                    result
                        .map(|effective| effective.composed_value)
                        .map_err(|error| error.to_string()),
                )
            }),
    );
    resolved.sort_by(|(left, _), (right, _)| left.cmp(right));
    embed_view_results(composed_value, resolved)
}

/// Embed the views of an already-resolved effective surface while retaining
/// the engine-owned effective identity of the surface and every successfully
/// resolved view. The composed surface and each embedded view keep their
/// existing presentation shape; identity travels only through the typed
/// sidecar returned here.
///
/// The caller must resolve `effective_surface` inside the same checked
/// generation supplied here. Holding the generation across both operations is
/// what makes the returned identity closure coherent.
pub fn embed_effective_surface_views_in_generation(
    generation: &CheckedEngineGeneration<'_>,
    project_root: Option<&Path>,
    effective_surface: &mut EffectiveItem,
) -> EmbeddedSurfaceViews {
    let surface = EffectiveUiItemIdentity::from_effective_item(effective_surface);
    if !effective_surface.composed_value.is_object() {
        return EmbeddedSurfaceViews {
            identity: EmbeddedSurfaceIdentity {
                request_engine_generation_identity: generation
                    .request_engine_generation_identity()
                    .to_string(),
                surface,
                views: BTreeMap::new(),
            },
            failures: Vec::new(),
        };
    }

    let mut resolved = Vec::new();
    let mut requests = Vec::new();
    let mut valid_refs = Vec::new();
    for view_ref in unique_view_refs(&effective_surface.composed_value) {
        match CanonicalRef::parse(&view_ref) {
            Ok(item_ref) => {
                valid_refs.push(view_ref);
                requests.push(EffectiveItemRequest {
                    item_ref,
                    expected_kind: Some("view".to_string()),
                    project_root: project_root.map(Path::to_path_buf),
                    subject_resolution_authority: SubjectResolutionAuthority::for_live_project_root(
                        project_root,
                    ),
                });
            }
            Err(error) => resolved.push((view_ref, Err(format!("invalid view ref: {error}")))),
        }
    }
    resolved.extend(
        valid_refs
            .into_iter()
            .zip(generation.effective_items(&requests))
            .map(|(view_ref, result)| {
                (
                    view_ref,
                    result
                        .map(|effective| {
                            let identity = EffectiveUiItemIdentity::from_effective_item(&effective);
                            (effective.composed_value, identity)
                        })
                        .map_err(|error| error.to_string()),
                )
            }),
    );
    resolved.sort_by(|(left, _), (right, _)| left.cmp(right));
    embed_identity_view_results(
        &mut effective_surface.composed_value,
        generation.request_engine_generation_identity().to_string(),
        surface,
        resolved,
    )
}

/// Descriptor-authority variant of
/// [`embed_effective_surface_views_in_generation`]. The typed content owner
/// remains borrowed across resolution of the complete surface/view closure.
pub fn embed_effective_surface_views_in_generation_under_project_authority(
    generation: &CheckedEngineGeneration<'_>,
    project_root: &Path,
    project_content: &dyn ryeos_engine::project_content::AuthoritativeProjectContent,
    effective_surface: &mut EffectiveItem,
) -> EmbeddedSurfaceViews {
    let surface = EffectiveUiItemIdentity::from_effective_item(effective_surface);
    if !effective_surface.composed_value.is_object() {
        return EmbeddedSurfaceViews {
            identity: EmbeddedSurfaceIdentity {
                request_engine_generation_identity: generation
                    .request_engine_generation_identity()
                    .to_string(),
                surface,
                views: BTreeMap::new(),
            },
            failures: Vec::new(),
        };
    }

    let mut resolved = Vec::new();
    let mut requests = Vec::new();
    let mut valid_refs = Vec::new();
    for view_ref in unique_view_refs(&effective_surface.composed_value) {
        match CanonicalRef::parse(&view_ref) {
            Ok(item_ref) => {
                valid_refs.push(view_ref);
                requests.push(EffectiveItemRequest {
                    item_ref,
                    expected_kind: Some("view".to_string()),
                    project_root: Some(project_root.to_path_buf()),
                    subject_resolution_authority: SubjectResolutionAuthority::LiveFs,
                });
            }
            Err(error) => resolved.push((view_ref, Err(format!("invalid view ref: {error}")))),
        }
    }
    resolved.extend(
        valid_refs
            .into_iter()
            .zip(generation.effective_items_under_project_authority(&requests, project_content))
            .map(|(view_ref, result)| {
                (
                    view_ref,
                    result
                        .map(|effective| {
                            let identity = EffectiveUiItemIdentity::from_effective_item(&effective);
                            (effective.composed_value, identity)
                        })
                        .map_err(|error| error.to_string()),
                )
            }),
    );
    resolved.sort_by(|(left, _), (right, _)| left.cmp(right));
    embed_identity_view_results(
        &mut effective_surface.composed_value,
        generation.request_engine_generation_identity().to_string(),
        surface,
        resolved,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

    use super::{
        EffectiveUiItemIdentity, EmbeddedSurfaceIdentity, EmbeddedViewIdentity,
        embed_identity_view_results, embed_views_with,
    };

    fn identity(
        canonical_ref: &str,
        digest_byte: char,
        effective_trust_class: TrustClass,
    ) -> EffectiveUiItemIdentity {
        EffectiveUiItemIdentity {
            canonical_ref: canonical_ref.to_string(),
            effective_definition_digest: EffectiveDefinitionDigest::parse(
                digest_byte.to_string().repeat(64),
            )
            .unwrap(),
            effective_trust_class,
        }
    }

    fn surface_binding_three_views() -> serde_json::Value {
        json!({
            "name": "test",
            "tiles": ["view:ryeos/chain/timeline"],
            "slots": {
                "bottom": { "content": "view:ryeos/input", "open": true }
            },
            "library": [
                { "group": "Threads", "views": ["view:ryeos/threads/list", "view:ryeos/chain/timeline"] }
            ]
        })
    }

    #[test]
    fn embeds_every_bound_view_once() {
        // Three unique refs across tiles/slots/library (one duplicated) →
        // three embedded bindings, each resolved exactly once.
        let mut surface = surface_binding_three_views();
        let mut resolved: Vec<String> = Vec::new();
        let failures = embed_views_with(&mut surface, |view_ref| {
            resolved.push(view_ref.to_string());
            Ok(json!({ "widget": "rows", "resolved_from": view_ref }))
        });

        assert!(failures.is_empty());
        assert_eq!(resolved.len(), 3, "duplicate refs resolve once");
        let views = surface["views"].as_object().unwrap();
        assert_eq!(views.len(), 3);
        for view_ref in [
            "view:ryeos/chain/timeline",
            "view:ryeos/input",
            "view:ryeos/threads/list",
        ] {
            assert_eq!(views[view_ref]["resolved_from"], view_ref);
        }
    }

    #[test]
    fn failed_ref_embeds_degraded_entry_others_still_embed() {
        // One failing ref records a per-view error and embeds a degraded
        // placeholder; the rest of the surface's views embed normally —
        // a single bad view never fails the whole surface.
        let mut surface = surface_binding_three_views();
        let failures = embed_views_with(&mut surface, |view_ref| {
            if view_ref == "view:ryeos/input" {
                Err("item not found".to_string())
            } else {
                Ok(json!({ "widget": "rows" }))
            }
        });

        assert_eq!(
            failures,
            vec![("view:ryeos/input".to_string(), "item not found".to_string())]
        );
        let views = surface["views"].as_object().unwrap();
        assert_eq!(views.len(), 3, "the failed ref is still keyed");
        assert_eq!(views["view:ryeos/input"]["degraded"], "item not found");
        assert_eq!(views["view:ryeos/threads/list"]["widget"], "rows");
        assert_eq!(views["view:ryeos/chain/timeline"]["widget"], "rows");
    }

    #[test]
    fn already_embedded_views_map_is_not_rewalked() {
        // Refs inside an existing `views` map are resolved bindings keyed
        // by ref, not refs to resolve — the walker must skip them.
        let mut surface = json!({
            "name": "test",
            "tiles": ["view:a/b"],
            "views": { "view:c/d": { "widget": "rows" } }
        });
        let mut resolved: Vec<String> = Vec::new();
        embed_views_with(&mut surface, |view_ref| {
            resolved.push(view_ref.to_string());
            Ok(json!({ "widget": "text" }))
        });
        assert_eq!(resolved, vec!["view:a/b"]);
        // The pre-existing entry survives alongside the new embedding.
        assert_eq!(surface["views"]["view:c/d"]["widget"], "rows");
        assert_eq!(surface["views"]["view:a/b"]["widget"], "text");
    }

    #[test]
    fn malformed_non_object_views_resets_before_embedding() {
        // `views` is never authored inline; a malformed non-object there
        // must not panic the embedder — it resets to a map and embeds.
        let mut surface = json!({ "name": "x", "tiles": ["view:a/b"], "views": "bogus" });
        let failures = embed_views_with(&mut surface, |_| Ok(json!({ "widget": "rows" })));
        assert!(failures.is_empty());
        assert_eq!(surface["views"]["view:a/b"]["widget"], "rows");
    }

    #[test]
    fn surface_without_view_refs_gains_no_views_map() {
        let mut surface = json!({ "name": "empty" });
        let failures = embed_views_with(&mut surface, |_| unreachable!("no refs to resolve"));
        assert!(failures.is_empty());
        assert!(surface.get("views").is_none());
    }

    #[test]
    fn non_object_composed_value_is_untouched() {
        let mut value = json!("view:not/an/object");
        let failures = embed_views_with(&mut value, |_| unreachable!("nothing to embed into"));
        assert!(failures.is_empty());
        assert_eq!(value, json!("view:not/an/object"));
    }

    #[test]
    fn identity_sidecar_retains_surface_and_resolved_view_identity() {
        let mut surface = json!({ "tiles": ["view:test/one"] });
        let surface_identity = identity("surface:test/base", 'a', TrustClass::TrustedBundle);
        let view_identity = identity("view:test/one", 'b', TrustClass::UntrustedProject);

        let embedded = embed_identity_view_results(
            &mut surface,
            "engine-generation-a".to_string(),
            surface_identity.clone(),
            vec![(
                "view:test/one".to_string(),
                Ok((json!({ "widget": "rows" }), view_identity.clone())),
            )],
        );

        assert!(embedded.failures.is_empty());
        assert_eq!(surface["views"]["view:test/one"]["widget"], "rows");
        assert_eq!(
            view_identity.effective_trust_class,
            TrustClass::UntrustedProject,
            "the binding compiler needs the engine's explicit effective trust result"
        );
        assert_eq!(
            embedded.identity,
            EmbeddedSurfaceIdentity {
                request_engine_generation_identity: "engine-generation-a".to_string(),
                surface: surface_identity,
                views: BTreeMap::from([(
                    "view:test/one".to_string(),
                    EmbeddedViewIdentity::Resolved {
                        identity: view_identity,
                    },
                )]),
            }
        );
        assert!(
            surface.get("effective_definition_digest").is_none(),
            "identity must stay out of authored presentation JSON"
        );
    }

    #[test]
    fn identity_sidecar_records_degraded_view_without_fabricating_identity() {
        let mut surface = json!({ "tiles": ["view:test/good", "view:test/bad"] });
        let good_identity = identity("view:test/good", 'b', TrustClass::TrustedProject);

        let embedded = embed_identity_view_results(
            &mut surface,
            "engine-generation-a".to_string(),
            identity("surface:test/base", 'a', TrustClass::TrustedBundle),
            vec![
                (
                    "view:test/bad".to_string(),
                    Err("item not found".to_string()),
                ),
                (
                    "view:test/good".to_string(),
                    Ok((json!({ "widget": "text" }), good_identity.clone())),
                ),
            ],
        );

        assert_eq!(
            embedded.failures,
            vec![("view:test/bad".to_string(), "item not found".to_string())]
        );
        assert_eq!(
            surface["views"]["view:test/bad"]["degraded"],
            "item not found"
        );
        assert_eq!(surface["views"]["view:test/good"]["widget"], "text");
        assert_eq!(
            embedded.identity.views["view:test/bad"],
            EmbeddedViewIdentity::Degraded {
                reason: "item not found".to_string(),
            }
        );
        assert_eq!(
            embedded.identity.views["view:test/good"],
            EmbeddedViewIdentity::Resolved {
                identity: good_identity,
            }
        );
    }
}
